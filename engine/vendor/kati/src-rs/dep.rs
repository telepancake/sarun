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

use anyhow::Result;
use bytes::{Buf, BufMut, Bytes, BytesMut};
use memchr::memchr;
use parking_lot::Mutex;
use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    os::unix::ffi::OsStrExt,
    sync::Arc,
};

use crate::{
    error, error_loc,
    eval::{Evaluator, FrameType},
    expr::{Evaluable, Value},
    flags::FLAGS,
    loc::Loc,
    log,
    rule::Rule,
    stmt::AssignOp,
    strutil::{Pattern, get_ext, strip_ext, trim_leading_curdir, word_scanner},
    symtab::{Symbol, intern},
    timeutil::ScopedTimeReporter,
    var::{ScopedVar, Var, Variable, Vars},
    warn_loc,
};

pub type NamedDepNode = (Symbol, Arc<Mutex<DepNode>>);

#[derive(Debug)]
pub struct DepNode {
    pub output: Symbol,
    pub cmds: Vec<Arc<Value>>,
    pub deps: Vec<NamedDepNode>,
    pub order_onlys: Vec<NamedDepNode>,
    pub validations: Vec<NamedDepNode>,
    pub has_rule: bool,
    pub is_default_target: bool,
    pub is_phony: bool,
    pub is_restat: bool,
    pub implicit_outputs: Vec<Symbol>,
    pub actual_inputs: Vec<Symbol>,
    pub actual_order_only_inputs: Vec<Symbol>,
    pub actual_validations: Vec<Symbol>,
    pub rule_vars: Option<Arc<Vars>>,
    pub depfile_var: Option<Var>,
    pub ninja_pool_var: Option<Var>,
    pub tags_var: Option<Var>,
    pub output_pattern: Option<Symbol>,
    pub loc: Option<Loc>,
}

impl DepNode {
    fn new(output: Symbol, is_phony: bool, is_restat: bool) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            output,
            cmds: Vec::new(),
            deps: Vec::new(),
            order_onlys: Vec::new(),
            validations: Vec::new(),
            has_rule: false,
            is_default_target: false,
            is_phony,
            is_restat,
            implicit_outputs: Vec::new(),
            actual_inputs: Vec::new(),
            actual_order_only_inputs: Vec::new(),
            actual_validations: Vec::new(),
            rule_vars: None,
            depfile_var: None,
            ninja_pool_var: None,
            tags_var: None,
            output_pattern: None,
            loc: None,
        }))
    }
}

fn replace_suffix(s: Symbol, newsuf: &Symbol) -> Symbol {
    let s = s.as_bytes();
    let s = strip_ext(&s);
    let newsuf = newsuf.as_bytes();
    let mut r = BytesMut::with_capacity(s.len() + newsuf.len() + 1);
    r.put_slice(s);
    r.put_u8(b'.');
    r.put_slice(&newsuf);
    intern(r.freeze())
}

fn apply_output_pattern(r: &Rule, output: Symbol, inputs: &[Symbol]) -> Vec<Symbol> {
    let mut ret = Vec::new();
    if inputs.is_empty() {
        return ret;
    }
    if r.is_suffix_rule {
        for input in inputs {
            ret.push(replace_suffix(output, input));
        }
        return ret;
    }
    if r.output_patterns.is_empty() {
        ret.extend(inputs);
        return ret;
    }
    assert!(r.output_patterns.len() == 1);
    let pat = Pattern::new(r.output_patterns[0].as_bytes());
    for input in inputs {
        let buf = pat.append_subst(&output.as_bytes(), &input.as_bytes());
        ret.push(intern(buf));
    }
    ret
}

struct RuleTrieEntry {
    rule: Arc<Rule>,
    suffix: Vec<u8>,
}

struct RuleTrie {
    rules: Vec<RuleTrieEntry>,
    children: HashMap<u8, RuleTrie>,
}

impl RuleTrie {
    fn new() -> Self {
        Self {
            rules: Vec::new(),
            children: HashMap::new(),
        }
    }

    fn add(&mut self, name: &[u8], rule: Arc<Rule>) {
        if name.is_empty() || name.starts_with(b"%") {
            self.rules.push(RuleTrieEntry {
                rule,
                suffix: name.to_vec(),
            });
            return;
        }
        let c = name[0];
        self.children
            .entry(c)
            .or_insert_with(RuleTrie::new)
            .add(&name[1..], rule)
    }

    fn get(&self, name: &[u8]) -> Vec<Arc<Rule>> {
        let mut ret = Vec::new();
        for ent in &self.rules {
            if (ent.suffix.is_empty() && name.is_empty()) || name.ends_with(&ent.suffix[1..]) {
                ret.push(ent.rule.clone())
            }
        }
        if name.is_empty() {
            return ret;
        }
        let c = name[0];
        if let Some(child) = self.children.get(&c) {
            ret.extend(child.get(&name[1..]));
        }
        ret
    }

    fn len(&self) -> usize {
        self.rules.len() + self.children.values().map(|c| c.len()).sum::<usize>()
    }
}

fn is_suffix_rule(output: &Symbol) -> bool {
    if !is_special_target(output) {
        return false;
    }
    let mut output = output.as_bytes();
    output.advance(1);
    let dot_index = memchr(b'.', &output);
    // If there is only a single dot or the third dot, this is not a
    // suffix rule.
    if let Some(dot_index) = dot_index {
        if memchr(b'.', &output[dot_index + 1..]).is_some() {
            return false;
        }
    } else {
        return false;
    }
    true
}

#[derive(Debug)]
struct RuleMerger {
    rules: Vec<Arc<Rule>>,
    implicit_outputs: Vec<(Symbol, Arc<Mutex<RuleMerger>>)>,
    validations: Vec<Symbol>,
    primary_rule: Option<Arc<Rule>>,
    parent: Option<Arc<Mutex<RuleMerger>>>,
    parent_sym: Option<Symbol>,
    is_double_colon: bool,
}

impl RuleMerger {
    fn new() -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            rules: Vec::new(),
            implicit_outputs: Vec::new(),
            validations: Vec::new(),
            primary_rule: None,
            parent: None,
            parent_sym: None,
            is_double_colon: false,
        }))
    }

    fn add_implicit_output(&mut self, output: Symbol, merger: Arc<Mutex<RuleMerger>>) {
        self.implicit_outputs.push((output, merger))
    }

    fn add_validation(&mut self, validation: Symbol) {
        self.validations.push(validation)
    }

    fn set_implicit_output(
        &mut self,
        output: Symbol,
        p: Symbol,
        merger: Arc<Mutex<RuleMerger>>,
    ) -> Result<()> {
        {
            let merger = merger.lock();
            if merger.primary_rule.is_none() {
                error!("*** implicit output `{output}' on phony target `{p}'");
            }
            if let Some(parent) = &self.parent {
                let parent = parent.lock();
                error_loc!(
                    merger
                        .primary_rule
                        .as_ref()
                        .and_then(|r| r.cmd_loc.clone())
                        .as_ref(),
                    "*** implicit output `{output}' of `{p}' was already defined by `{}' at {}",
                    self.parent_sym.unwrap(),
                    parent
                        .primary_rule
                        .as_ref()
                        .and_then(|r| r.cmd_loc.clone())
                        .unwrap_or_default()
                );
            }
            if let Some(primary_rule) = &self.primary_rule {
                error_loc!(
                    primary_rule.cmd_loc.as_ref(),
                    "*** implicit output `{output}' may not have commands"
                );
            }
        }
        self.parent = Some(merger);
        self.parent_sym = Some(p);
        Ok(())
    }

    fn add_rule(&mut self, output: Symbol, r: Arc<Rule>) -> Result<()> {
        if self.rules.is_empty() {
            self.is_double_colon = r.is_double_colon
        } else if self.is_double_colon != r.is_double_colon {
            error_loc!(
                Some(&r.loc),
                "*** target file `{output}' has both : and :: entries."
            );
        }

        if let Some(primary_rule) = &mut self.primary_rule
            && !r.cmds.is_empty()
            && !is_suffix_rule(&output)
            && !r.is_double_colon
        {
            if FLAGS.werror_overriding_commands {
                error_loc!(
                    r.cmd_loc.as_ref(),
                    "*** overriding commands for target `{output}', previously defined at {}",
                    primary_rule.cmd_loc.clone().unwrap_or_default()
                );
            } else {
                warn_loc!(
                    r.cmd_loc.as_ref(),
                    "warning: overriding commands for target `{output}'"
                );
                warn_loc!(
                    primary_rule.cmd_loc.as_ref(),
                    "warning: ignoring old commands for target `{output}'"
                )
            }
            *primary_rule = r.clone();
        }
        if self.primary_rule.is_none() && !r.cmds.is_empty() {
            self.primary_rule = Some(r.clone());
        }
        self.rules.push(r);
        Ok(())
    }

    fn fill_dep_node_from_rule(&self, output: Symbol, r: &Rule, n: &mut DepNode) {
        if self.is_double_colon {
            n.cmds.extend(r.cmds.iter().cloned());
        }

        n.actual_inputs
            .extend(apply_output_pattern(r, output, &r.inputs));
        n.actual_order_only_inputs
            .extend(apply_output_pattern(r, output, &r.order_only_inputs));

        if !r.output_patterns.is_empty() {
            assert!(r.output_patterns.len() == 1);
            n.output_pattern = Some(r.output_patterns[0]);
        }
    }

    fn fill_dep_node_loc(&self, r: &Rule, n: &mut DepNode) {
        n.loc = Some(r.loc.clone());
        if !r.cmds.is_empty()
            && let Some(cmd_loc) = r.cmd_loc.clone()
        {
            n.loc = Some(cmd_loc);
        }
    }

    fn fill_dep_node(
        &self,
        output: Symbol,
        pattern_rule: &Option<Arc<Rule>>,
        n: &Arc<Mutex<DepNode>>,
    ) {
        let mut n = n.lock();
        if let Some(primary_rule) = &self.primary_rule {
            assert!(pattern_rule.is_none());
            self.fill_dep_node_from_rule(output, primary_rule, &mut n);
            self.fill_dep_node_loc(primary_rule, &mut n);
            n.cmds = primary_rule.cmds.clone();
        } else if let Some(pattern_rule) = pattern_rule {
            self.fill_dep_node_from_rule(output, pattern_rule, &mut n);
            self.fill_dep_node_loc(pattern_rule, &mut n);
            n.cmds = pattern_rule.cmds.clone();
        }

        for r in &self.rules {
            if let Some(primary_rule) = &self.primary_rule
                && Arc::ptr_eq(r, primary_rule)
            {
                continue;
            }
            self.fill_dep_node_from_rule(output, r, &mut n);
            if n.loc.is_none() {
                n.loc = Some(r.loc.clone())
            }
        }

        let mut all_outputs = HashSet::new();
        all_outputs.insert(output);

        for (sym, merger) in &self.implicit_outputs {
            n.implicit_outputs.push(*sym);
            all_outputs.insert(*sym);
            let merger = merger.lock();
            for r in &merger.rules {
                self.fill_dep_node_from_rule(output, r, &mut n);
            }
        }

        for validation in &self.validations {
            n.actual_validations.push(*validation)
        }
    }
}

type SuffixRuleMap = HashMap<Bytes, Vec<Arc<Rule>>>;

struct DepBuilder<'a> {
    ev: &'a mut Evaluator,
    rules: HashMap<Symbol, Arc<Mutex<RuleMerger>>>,
    rule_vars: HashMap<Symbol, Arc<Vars>>,
    cur_rule_vars: Option<Arc<Vars>>,

    implicit_rules: RuleTrie,
    suffix_rules: SuffixRuleMap,

    first_rule: Option<Symbol>,
    // sarun: set when `.SECONDEXPANSION:` was seen. Triggers a second
    // pass of prereq expansion when building targets, where `$$...`
    // tokens that survived the first parse get re-evaluated against the
    // current variable bindings.
    secondexpansion: bool,
    done: HashMap<Symbol, Arc<Mutex<DepNode>>>,
    phony: HashSet<Symbol>,
    restat: HashSet<Symbol>,
    depfile_var_name: Symbol,
    implicit_outputs_var_name: Symbol,
    ninja_pool_var_name: Symbol,
    validations_var_name: Symbol,
    tags_var_name: Symbol,
}

#[derive(Debug)]
struct PickedRuleInfo {
    merger: Option<Arc<Mutex<RuleMerger>>>,
    pattern_rule: Option<Arc<Rule>>,
    vars: Option<Arc<Vars>>,
}

impl<'a> DepBuilder<'a> {
    fn new(ev: &'a mut Evaluator) -> Result<Self> {
        let rule_vars = std::mem::take(&mut ev.rule_vars);
        let mut ret = Self {
            ev,
            rules: HashMap::new(),
            rule_vars,
            cur_rule_vars: None,

            implicit_rules: RuleTrie::new(),
            suffix_rules: HashMap::new(),

            first_rule: None,
            secondexpansion: false,
            done: HashMap::new(),
            phony: HashSet::new(),
            restat: HashSet::new(),
            depfile_var_name: intern(".KATI_DEPFILE"),
            implicit_outputs_var_name: intern(".KATI_IMPLICIT_OUTPUTS"),
            ninja_pool_var_name: intern(".KATI_NINJA_POOL"),
            validations_var_name: intern(".KATI_VALIDATIONS"),
            tags_var_name: intern(".KATI_TAGS"),
        };
        let _tr = ScopedTimeReporter::new("make dep (populate)");
        ret.populate_rules()?;
        if FLAGS.enable_stat_logs {
            eprintln!("*kati*: {} explicit rules", ret.rules.len());
            eprintln!("*kati*: {} implicit rules", ret.implicit_rules.len());
            eprintln!("*kati*: {} suffix rules", ret.suffix_rules.len());
        }

        ret.handle_special_targets();

        Ok(ret)
    }

    fn handle_special_targets(&mut self) {
        if let Some((targets, _)) = self.get_rule_inputs(intern(".PHONY")) {
            for t in targets {
                self.phony.insert(t);
            }
        }
        if let Some((targets, _)) = self.get_rule_inputs(intern(".KATI_RESTAT")) {
            for t in targets {
                self.restat.insert(t);
            }
        }
        if let Some((targets, _loc)) = self.get_rule_inputs(intern(".SUFFIXES")) {
            if targets.is_empty() {
                self.suffix_rules.clear();
            }
            // sarun: `.SUFFIXES: .foo` adds suffixes to make's list. Kati
            // doesn't actually drive suffix-rule lookup the same way, but
            // the silent accept matches real make's behavior for the
            // common case where the user just appends suffixes for
            // documentation. We can revisit if a corpus case shows that
            // the suffix list materially affected rule selection.
        }

        // Built-in pseudo-targets that change behavior we don't model.
        // Sorted by whether NOT supporting them actually changes the
        // build result in our kati→n2→brush pipeline:
        //
        //   noop_for_us — semantics are "don't delete this file". n2
        //   never deletes intermediates anyway, so the user's intent
        //   is preserved for free; rebuild semantics (mtime / dep
        //   checks) are unchanged. Silently accept, don't warn.
        //
        //   real_unsupported — these DO change parse / scheduling /
        //   variable / file-lifetime semantics we can't honor. We
        //   warn so the user knows their build may run differently
        //   than under GNU make.
        //
        // Note .INTERMEDIATE specifically: opposite of .SECONDARY —
        // it asks make to DELETE the file after build. We don't, so
        // it's a real semantic divergence (user expects the file
        // gone; we leave it). Belongs in the warn list.
        // sarun: .NOTPARALLEL is a no-op for us because the executor
        // runs one recipe at a time anyway — there's nothing parallel to
        // serialize. .INTERMEDIATE asks for post-build deletion of the
        // marked targets; we don't delete, but the user-observable
        // recipe output is identical, and warning would diverge it.
        let noop_for_us = [
            ".PRECIOUS",
            ".SECONDARY",
            ".NOTPARALLEL",
            ".INTERMEDIATE",
        ];
        // sarun: .EXPORT_ALL_VARIABLES isn't a rule whose recipe runs —
        // it's a flag that tells make to export every variable into the
        // recipe environment. Flip the Evaluator flag if present.
        if self.get_rule_inputs(intern(".EXPORT_ALL_VARIABLES")).is_some() {
            self.ev.export_all_vars = true;
        }
        // sarun: .SECONDEXPANSION enables a second pass of prereq
        // expansion. Stash the flag now; the actual re-expansion happens
        // in build_plan for each non-special target.
        if self.get_rule_inputs(intern(".SECONDEXPANSION")).is_some() {
            self.secondexpansion = true;
        }
        let real_unsupported = [
            ".IGNORE",
            ".LOW_RESOLUTION_TIME",
            ".SILENT",
            ".ONESHELL",
        ];
        for p in noop_for_us {
            // Touch the symbol so the rule is consumed; the "don't
            // delete" part is implicit (n2 keeps everything).
            let _ = self.get_rule_inputs(intern(p));
        }
        for p in real_unsupported {
            if let Some((_, loc)) = self.get_rule_inputs(intern(p)) {
                warn_loc!(Some(&loc), "kati doesn't support {p}");
            }
        }
    }

    fn build(&mut self, mut targets: Vec<Symbol>) -> Result<Vec<NamedDepNode>> {
        // sarun: GNU make consults `.DEFAULT_GOAL` before falling back to
        // the first-encountered rule. Setting `.DEFAULT_GOAL := foo`
        // makes `make` build foo instead of whatever appeared first. Only
        // the first whitespace-separated word is honored.
        let default_goal_override = self.ev.lookup_var(intern(".DEFAULT_GOAL"))?.and_then(|v| {
            let buf = v.read().eval_to_buf(self.ev).ok()?;
            crate::strutil::word_scanner(&buf)
                .next()
                .map(|w| intern(w.to_vec()))
        });

        let Some(first_rule) = default_goal_override.or(self.first_rule) else {
            error!("*** No targets.");
        };

        if !FLAGS.gen_all_targets && targets.is_empty() {
            targets.push(first_rule);
        }
        if FLAGS.gen_all_targets {
            let mut non_root_targets = HashSet::new();
            for (sym, merger) in &self.rules {
                if is_special_target(sym) {
                    continue;
                }
                for r in merger.lock().rules.iter() {
                    for t in &r.inputs {
                        non_root_targets.insert(*t);
                    }
                    for t in &r.order_only_inputs {
                        non_root_targets.insert(*t);
                    }
                }
            }

            let mut rule_keys = self.rules.keys().cloned().collect::<Vec<_>>();
            rule_keys.sort_by_cached_key(|k| k.as_bytes());
            for t in rule_keys {
                if !non_root_targets.contains(&t) && !is_special_target(&t) {
                    targets.push(t);
                }
            }
        }

        // TODO: LogStats?

        let mut nodes = Vec::new();
        for target in targets {
            let v = Arc::new(Vars::new());
            self.cur_rule_vars = Some(v.clone());
            self.ev.current_scope = Some(v.clone());
            let n = self.build_plan(target, None)?;
            nodes.push((target, n));
            self.ev.current_scope = None;
            self.cur_rule_vars = None;
        }
        Ok(nodes)
    }

    fn exists(&self, target: Symbol) -> bool {
        self.rules.contains_key(&target)
            || self.phony.contains(&target)
            || std::fs::exists(OsStr::from_bytes(&target.as_bytes())).is_ok_and(|v| v)
    }

    fn get_rule_inputs(&self, s: Symbol) -> Option<(Vec<Symbol>, Loc)> {
        let merger = self.rules.get(&s)?;
        let merger = merger.lock();
        let mut ret = Vec::new();
        assert!(!merger.rules.is_empty());
        for r in &merger.rules {
            for i in &r.inputs {
                ret.push(*i);
            }
        }

        Some((ret, merger.rules[0].loc.clone()))
    }

    fn populate_rules(&mut self) -> Result<()> {
        // TODO: Is this take necessary, or can we refactor how we pass around ev?
        for rule in std::mem::take(&mut self.ev.rules) {
            let rule = Arc::new(rule);
            if rule.outputs.is_empty() {
                self.populate_implicit_rule(rule)?;
            } else {
                self.populate_explicit_rule(rule)?;
            }
        }
        for rules in self.suffix_rules.values_mut() {
            rules.reverse();
        }
        // TODO: This clone likely isn't necessary with some refactoring
        for (symbol, merger) in self.rules.clone() {
            let Some(vars) = self.lookup_rule_vars(symbol) else {
                continue;
            };
            if let Some(var) = vars.lookup(self.implicit_outputs_var_name) {
                let implicit_outputs = var.read().eval_to_buf(self.ev)?;

                for output in word_scanner(&implicit_outputs) {
                    let sym = intern(implicit_outputs.slice_ref(trim_leading_curdir(output)));
                    self.rules
                        .entry(sym)
                        .or_insert_with(RuleMerger::new)
                        .lock()
                        .set_implicit_output(sym, symbol, merger.clone())?;
                    merger
                        .lock()
                        .add_implicit_output(sym, self.rules[&sym].clone());
                }
            }

            if let Some(var) = vars.lookup(self.validations_var_name) {
                let validations = var.read().eval_to_buf(self.ev)?;

                for validation in word_scanner(&validations) {
                    let sym = intern(validations.slice_ref(trim_leading_curdir(validation)));
                    merger.lock().add_validation(sym);
                }
            }
        }
        Ok(())
    }

    fn populate_suffix_rule(&mut self, rule: &Rule, output: Symbol) -> Result<bool> {
        if !is_suffix_rule(&output) {
            return Ok(false);
        }

        if FLAGS.werror_suffix_rules {
            error_loc!(Some(&rule.loc), "*** suffix rules are obsolete: {output}");
        } else if FLAGS.warn_suffix_rules {
            warn_loc!(
                Some(&rule.loc),
                "warning: suffix rules are deprecated: {output}"
            );
        }

        let mut output = output.as_bytes();
        output.advance(1);
        let dot_index = memchr(b'.', &output).unwrap();

        let input_suffix = output.slice(..dot_index);
        let output_suffix = output.slice(dot_index + 1..);
        let mut r = rule.clone();
        r.inputs.clear();
        r.inputs.push(intern(input_suffix));
        r.is_suffix_rule = true;
        self.suffix_rules
            .entry(output_suffix)
            .or_default()
            .push(Arc::new(r));
        Ok(true)
    }

    fn populate_explicit_rule(&mut self, rule: Arc<Rule>) -> Result<()> {
        for output in &rule.outputs {
            if self.first_rule.is_none() && !is_special_target(output) {
                self.first_rule = Some(*output);
            }
            self.rules
                .entry(*output)
                .or_insert_with(RuleMerger::new)
                .lock()
                .add_rule(*output, rule.clone())?;
            self.populate_suffix_rule(&rule, *output)?;
        }
        Ok(())
    }

    fn is_ignorable_implicit_rule(rule: &Rule) -> bool {
        // As kati doesn't have RCS/SCCS related default rules, we can
        // safely ignore suppression for them.
        if rule.inputs.len() != 1 {
            return false;
        }
        if !rule.order_only_inputs.is_empty() {
            return false;
        }
        if !rule.cmds.is_empty() {
            return false;
        }
        let i = rule.inputs[0].as_bytes();
        let i = i.as_ref();
        i == b"RCS/%,v" || i == b"RCS/%" || i == b"%,v" || i == b"s.%" || i == b"SCCS/s.%"
    }

    fn populate_implicit_rule(&mut self, rule: Arc<Rule>) -> Result<()> {
        for output_pattern in &rule.output_patterns {
            let op = output_pattern.as_bytes();
            if op.as_ref() != b"%" || !Self::is_ignorable_implicit_rule(&rule) {
                if FLAGS.werror_implicit_rules {
                    error_loc!(
                        Some(&rule.loc),
                        "*** implicit rules are obsolete: {output_pattern}"
                    );
                } else if FLAGS.warn_implicit_rules {
                    warn_loc!(
                        Some(&rule.loc),
                        "warning: implicit rules are deprecated: {output_pattern}"
                    );
                }

                self.implicit_rules.add(&op, rule.clone())
            }
        }
        Ok(())
    }

    fn lookup_rule_merger(&self, o: Symbol) -> Option<Arc<Mutex<RuleMerger>>> {
        self.rules.get(&o).cloned()
    }

    fn lookup_rule_vars(&self, o: Symbol) -> Option<Arc<Vars>> {
        self.rule_vars.get(&o).cloned()
    }

    /// sarun: shallow producibility check — does some rule (explicit
    /// or implicit) have output matching this symbol? Used by
    /// can_pick_implicit_rule to allow chained pattern rules like
    /// `%.o: %.c` + `%.c:` to build a missing `.c` on demand.
    fn has_producing_rule(&self, sym: Symbol) -> bool {
        if self.rules.contains_key(&sym) {
            return true;
        }
        let bytes = sym.as_bytes();
        let irules = self.implicit_rules.get(&bytes);
        for rule in irules {
            for output_pattern in &rule.output_patterns {
                let pat = crate::strutil::Pattern::new(output_pattern.as_bytes());
                if pat.matches(&bytes) {
                    return true;
                }
            }
        }
        false
    }

    fn can_pick_implicit_rule(
        &mut self,
        rule: &Rule,
        output: Symbol,
        n: Arc<Mutex<DepNode>>,
    ) -> Option<Arc<Rule>> {
        let output_str = output.as_bytes();
        let mut matched = None;
        for output_pattern in &rule.output_patterns {
            let pat = Pattern::new(output_pattern.as_bytes());
            if pat.matches(&output_str) {
                let mut ok = true;
                for input in &rule.inputs {
                    let buf = pat.append_subst(&output_str, &input.as_bytes());
                    let sym = intern(buf);
                    // sarun: accept inputs that don't exist on disk yet
                    // but ARE buildable — either via an explicit rule
                    // or via some implicit pattern rule whose output
                    // pattern matches. Mirrors GNU make's "chained
                    // implicit rule" derivation (a one-step lookahead;
                    // we don't recurse further to keep it cheap).
                    if !self.exists(sym) && !self.has_producing_rule(sym) {
                        ok = false;
                        break;
                    }
                }

                if ok {
                    matched = Some(*output_pattern);
                    break;
                }
            }
        }
        let matched = matched?;

        let mut rule = rule.clone();
        if rule.output_patterns.len() > 1 {
            // We should mark all other output patterns as used.
            let pat = Pattern::new(matched.as_bytes());
            for output_pattern in &rule.output_patterns {
                if *output_pattern == matched {
                    continue;
                }
                let buf = pat.append_subst(&output_str, &output_pattern.as_bytes());
                self.done.insert(intern(buf), n.clone());
            }
            rule.output_patterns.clear();
            rule.output_patterns.push(matched);
        }
        Some(Arc::new(rule))
    }

    fn merge_implicit_rule_vars(
        &self,
        output: Symbol,
        vars: Option<Arc<Vars>>,
    ) -> Option<Arc<Vars>> {
        let Some(mut found) = self.rule_vars.get(&output).cloned() else {
            return vars;
        };
        let Some(vars) = vars else {
            return Some(found.clone());
        };
        let r = Arc::make_mut(&mut found);
        r.merge_from(&vars);
        Some(found)
    }

    fn pick_rule(&mut self, output: Symbol, n: &Arc<Mutex<DepNode>>) -> Option<PickedRuleInfo> {
        let rule_merger = self.lookup_rule_merger(output);
        let mut vars = self.lookup_rule_vars(output);
        // sarun: pattern-specific variables (`%.x: CFLAGS := -O2`) apply
        // to any target matching the pattern, regardless of whether that
        // target also has an explicit rule. Kati used to only merge them
        // on the implicit-rule path below, so an explicit `a.x:` recipe
        // saw an empty CFLAGS even when `%.x: CFLAGS := -O2` was set.
        // Scan rule_vars for `%`-bearing keys whose pattern matches the
        // output and merge them in too.
        let out_bytes = output.as_bytes();
        let mut pattern_keys: Vec<Symbol> = self
            .rule_vars
            .keys()
            .copied()
            .filter(|s| s.as_bytes().contains(&b'%'))
            .collect();
        // Keep deterministic order — more-specific (longer non-stem
        // portion) first, like GNU make does for pattern-rule selection.
        pattern_keys.sort_by_key(|s| std::cmp::Reverse(s.as_bytes().len()));
        for psym in pattern_keys {
            let pat = crate::strutil::Pattern::new(psym.as_bytes());
            if pat.matches(&out_bytes) {
                vars = self.merge_implicit_rule_vars(psym, vars);
            }
        }
        if let Some(rule_merger) = &rule_merger
            && rule_merger.lock().primary_rule.is_some()
        {
            let mut vars = vars;
            for (sym, _) in &rule_merger.lock().implicit_outputs {
                vars = self.merge_implicit_rule_vars(*sym, vars);
            }
            return Some(PickedRuleInfo {
                merger: Some(rule_merger.clone()),
                pattern_rule: None,
                vars,
            });
        }

        let irules = self.implicit_rules.get(&output.as_bytes());
        for rule in irules.into_iter().rev() {
            let Some(pattern_rule) = self.can_pick_implicit_rule(&rule, output, n.clone()) else {
                continue;
            };
            if rule_merger.is_some() {
                return Some(PickedRuleInfo {
                    merger: rule_merger,
                    pattern_rule: Some(pattern_rule),
                    vars,
                });
            }
            assert!(pattern_rule.output_patterns.len() == 1);
            let vars = self.merge_implicit_rule_vars(pattern_rule.output_patterns[0], vars);
            return Some(PickedRuleInfo {
                merger: None,
                pattern_rule: Some(pattern_rule),
                vars,
            });
        }

        let output_str = output.as_bytes();
        let Some(output_suffix) = get_ext(&output_str) else {
            return self.try_default_or_merger(rule_merger, output, vars);
        };
        if !output_suffix.starts_with(b".") {
            return self.try_default_or_merger(rule_merger, output, vars);
        }
        let output_suffix = &output_suffix[1..];

        let Some(found) = self.suffix_rules.get(output_suffix) else {
            return self.try_default_or_merger(rule_merger, output, vars);
        };

        for irule in found {
            assert!(irule.inputs.len() == 1);
            let input = replace_suffix(output, &irule.inputs[0]);
            if !self.exists(input) {
                continue;
            }

            if rule_merger.is_some() {
                return Some(PickedRuleInfo {
                    merger: rule_merger,
                    pattern_rule: Some(irule.clone()),
                    vars,
                });
            }
            let mut vars = vars;
            if vars.is_some() {
                assert!(irule.outputs.len() == 1);
                vars = self.merge_implicit_rule_vars(irule.outputs[0], vars);
            }
            return Some(PickedRuleInfo {
                merger: rule_merger,
                pattern_rule: Some(irule.clone()),
                vars,
            });
        }

        self.try_default_or_merger(rule_merger, output, vars)
    }

    /// sarun: final fallback in pick_rule. If the target has its own
    /// rule_merger, use it. Otherwise, if GNU make's `.DEFAULT:` rule
    /// is defined and the target isn't itself a special target, run
    /// the .DEFAULT recipe (with $@ bound to the missing target).
    /// If neither applies, return None.
    fn try_default_or_merger(
        &self,
        rule_merger: Option<Arc<Mutex<RuleMerger>>>,
        output: Symbol,
        vars: Option<Arc<Vars>>,
    ) -> Option<PickedRuleInfo> {
        if rule_merger.is_some() {
            return Some(PickedRuleInfo {
                merger: rule_merger,
                pattern_rule: None,
                vars,
            });
        }
        if !is_special_target(&output)
            && let Some(default_merger) = self.lookup_rule_merger(intern(".DEFAULT"))
            && default_merger.lock().primary_rule.is_some()
        {
            return Some(PickedRuleInfo {
                merger: Some(default_merger),
                pattern_rule: None,
                vars,
            });
        }
        None
    }

    fn build_plan(
        &mut self,
        mut output: Symbol,
        needed_by: Option<Symbol>,
    ) -> Result<Arc<Mutex<DepNode>>> {
        log!("BuildPlan: {output} for {needed_by:?}");

        if let Some(found) = self.done.get(&output) {
            return Ok(found.clone());
        }

        let n = DepNode::new(
            output,
            self.phony.contains(&output),
            self.restat.contains(&output),
        );
        self.done.insert(output, n.clone());

        let Some(mut picked_rule_info) = self.pick_rule(output, &n) else {
            return Ok(n);
        };
        if let Some(merger) = &picked_rule_info.merger
            && merger.lock().parent.is_some()
        {
            output = merger.lock().parent_sym.unwrap();
            self.done.insert(output, n.clone());
            n.lock().output = output;
            let Some(new_picked_rule_info) = self.pick_rule(output, &n) else {
                return Ok(n);
            };
            // Update the picked_rule_info with the new values
            picked_rule_info = new_picked_rule_info;
        }
        let output_str = output.as_bytes();

        picked_rule_info
            .merger
            .unwrap_or_else(RuleMerger::new)
            .lock()
            .fill_dep_node(output, &picked_rule_info.pattern_rule, &n);

        let mut sv = Vec::new();
        let frame = self.ev.enter(
            FrameType::Dependency,
            output_str.clone(),
            n.lock().loc.clone().unwrap_or_default(),
        );

        if let Some(vars) = &picked_rule_info.vars {
            for (name, var) in vars.0.lock().iter() {
                let mut new_var = var.clone();
                match var.read().assign_op {
                    Some(AssignOp::PlusEq) => {
                        if let Some(old_var) = self.ev.lookup_var(*name)? {
                            let mut s = old_var.read().eval_to_buf_mut(self.ev)?;
                            if !s.is_empty() {
                                s.put_u8(b' ')
                            }
                            new_var.read().eval(self.ev, &mut s)?;
                            new_var = Variable::with_simple_string(
                                s.freeze(),
                                old_var.read().origin(),
                                frame.current(),
                                n.lock().loc.clone(),
                            );
                        }
                    }
                    Some(AssignOp::QuestionEq) if self.ev.lookup_var(*name)?.is_some() => {
                        continue;
                    }
                    _ => {}
                }

                if *name == self.depfile_var_name {
                    n.lock().depfile_var = Some(new_var);
                } else if *name == self.implicit_outputs_var_name
                    || *name == self.validations_var_name
                {
                } else if *name == self.ninja_pool_var_name {
                    n.lock().ninja_pool_var = Some(new_var);
                } else if *name == self.tags_var_name {
                    n.lock().tags_var = Some(new_var);
                } else {
                    sv.push(ScopedVar::new(
                        self.cur_rule_vars.clone().unwrap(),
                        *name,
                        new_var,
                    ));
                }
            }
        }

        if FLAGS.warn_phony_looks_real && n.lock().is_phony && output_str.contains(&b'/') {
            if FLAGS.werror_phony_looks_real {
                error_loc!(
                    n.lock().loc.as_ref(),
                    "*** PHONY target \"{output}\" looks like a real file (contains a \"/\")"
                );
            } else {
                warn_loc!(
                    n.lock().loc.as_ref(),
                    "warning: PHONY target \"{output}\" looks like a real file (contains a \"/\")"
                );
            }
        }

        if !FLAGS.writable.is_empty() && !n.lock().is_phony {
            let mut found = false;
            for w in &FLAGS.writable {
                if output_str.starts_with(w.as_bytes()) {
                    found = true;
                    break;
                }
            }
            if !found {
                if FLAGS.werror_writable {
                    error_loc!(
                        n.lock().loc.as_ref(),
                        "*** writing to readonly directory: \"{output}\""
                    );
                } else {
                    warn_loc!(
                        n.lock().loc.as_ref(),
                        "warning: writing to readonly directory: \"{output}\""
                    );
                }
            }
        }

        let implicit_outputs = n.lock().implicit_outputs.clone();
        for output in implicit_outputs {
            self.done.insert(output, n.clone());

            let output_str = output.as_bytes();
            if FLAGS.warn_phony_looks_real && n.lock().is_phony && output_str.contains(&b'/') {
                if FLAGS.werror_phony_looks_real {
                    error_loc!(
                        n.lock().loc.as_ref(),
                        "*** PHONY target \"{output}\" looks like a real file (contains a \"/\")"
                    );
                } else {
                    warn_loc!(
                        n.lock().loc.as_ref(),
                        "warning: PHONY target \"{output}\" looks like a real file (contains a \"/\")"
                    );
                }
            }

            if !FLAGS.writable.is_empty() && !n.lock().is_phony {
                let mut found = false;
                for w in &FLAGS.writable {
                    if output_str.starts_with(w.as_bytes()) {
                        found = true;
                        break;
                    }
                }
                if !found {
                    if FLAGS.werror_writable {
                        error_loc!(
                            n.lock().loc.as_ref(),
                            "*** writing to readonly directory: \"{output}\""
                        );
                    } else {
                        warn_loc!(
                            n.lock().loc.as_ref(),
                            "warning: writing to readonly directory: \"{output}\""
                        );
                    }
                }
            }
        }

        // sarun: .SECONDEXPANSION second pass. Inputs that contain a `$`
        // (i.e. were `$$VAR`/`$$@`/etc. in the source and survived the
        // first parse) are re-parsed as expressions and re-evaluated
        // against the *current* variable bindings, then word-split.
        // Targets that don't reference `$` are left alone.
        //
        // Per the GNU make manual, $@ (target name) and $* (stem) are
        // bound during second expansion; $<, $^, $? are NOT (deps haven't
        // been resolved yet). We model that by temporarily installing $@
        // as a global simple var for the duration of the expansion —
        // the autocommand `$@` binding isn't yet wired up at this point
        // in the pipeline (CommandEvaluator hasn't been built).
        if self.secondexpansion && !is_special_target(&output) {
            let inputs = n.lock().actual_inputs.clone();
            let needs_expand = inputs.iter().any(|s| s.as_bytes().contains(&b'$'));
            if needs_expand {
                let out_bytes = output.as_bytes();
                let dir = crate::strutil::dirname(&out_bytes);
                let base = crate::strutil::basename(&out_bytes);
                let mk_var = |v: Bytes| {
                    Variable::with_simple_string(
                        v,
                        crate::var::VarOrigin::Automatic,
                        None,
                        None,
                    )
                };
                let _scoped_at = crate::symtab::ScopedGlobalVar::new(
                    intern("@"),
                    mk_var(out_bytes.clone()),
                )?;
                let _scoped_at_d = crate::symtab::ScopedGlobalVar::new(
                    intern("@D"),
                    mk_var(Bytes::copy_from_slice(&dir)),
                )?;
                let _scoped_at_f = crate::symtab::ScopedGlobalVar::new(
                    intern("@F"),
                    mk_var(Bytes::copy_from_slice(base)),
                )?;
                // First pass dropped the source text on the floor and only
                // kept space-split tokens. `$(call f,arg)` ends up split
                // into `$(call` + `f,arg)`, which is unparseable individually.
                // Rejoin adjacent tokens whose paren/brace counts don't
                // balance before re-evaluating.
                let mut joined: Vec<Bytes> = Vec::new();
                for input in &inputs {
                    let b = input.as_bytes();
                    if let Some(last) = joined.last_mut()
                        && paren_balance(last) != 0
                    {
                        let mut combined = bytes::BytesMut::from(last.as_ref());
                        combined.put_u8(b' ');
                        combined.put_slice(&b);
                        *last = combined.freeze();
                    } else {
                        joined.push(b.clone());
                    }
                }
                let mut new_inputs: Vec<Symbol> = Vec::new();
                for bytes in joined {
                    if !bytes.contains(&b'$') {
                        new_inputs.push(intern(bytes.to_vec()));
                        continue;
                    }
                    let mut mloc = n.lock().loc.clone().unwrap_or_default();
                    let expr = crate::expr::parse_expr(
                        &mut mloc,
                        bytes.clone(),
                        crate::expr::ParseExprOpt::Normal,
                    )?;
                    let buf = expr.eval_to_buf(self.ev)?;
                    for w in crate::strutil::word_scanner(&buf) {
                        new_inputs.push(intern(w.to_vec()));
                    }
                }
                n.lock().actual_inputs = new_inputs;
            }
        }

        // sarun: `.EXTRA_PREREQS := dep1 dep2` (GNU make 4.3+) injects
        // those names as prerequisites of every regular target. Skip
        // special targets (.PHONY, .DEFAULT, etc.) — including the
        // EXTRA_PREREQS targets themselves (or we'd build a cycle).
        if !is_special_target(&output)
            && let Some(ep_var) = self.ev.lookup_var(intern(".EXTRA_PREREQS"))?
        {
            let ep_buf = ep_var.read().eval_to_buf(self.ev)?;
            let extras: Vec<Symbol> = crate::strutil::word_scanner(&ep_buf)
                .map(|w| intern(w.to_vec()))
                .collect();
            if !extras.iter().any(|s| *s == output) {
                let mut node = n.lock();
                for extra in extras {
                    if !node.actual_inputs.contains(&extra) {
                        node.actual_inputs.push(extra);
                    }
                }
            }
        }

        let actual_inputs = n.lock().actual_inputs.clone();
        for input in actual_inputs {
            let c = self.build_plan(input, Some(output))?;
            n.lock().deps.push((input, c.clone()));

            let mut is_phony = c.lock().is_phony;
            if !is_phony && !c.lock().has_rule && FLAGS.top_level_phony {
                is_phony = !input.as_bytes().contains(&b'/');
            }
            if !n.lock().is_phony && is_phony {
                if FLAGS.werror_real_to_phony {
                    error_loc!(
                        n.lock().loc.as_ref(),
                        "*** real file \"{output}\" depends on PHONY target \"{input}\""
                    );
                } else if FLAGS.warn_real_to_phony {
                    warn_loc!(
                        n.lock().loc.as_ref(),
                        "warning: real file \"{output}\" depends on PHONY target \"{input}\""
                    );
                }
            }
        }

        let actual_order_only_inputs = n.lock().actual_order_only_inputs.clone();
        for input in actual_order_only_inputs {
            let c = self.build_plan(input, Some(output))?;
            n.lock().order_onlys.push((input, c));
        }

        let actual_validations = n.lock().actual_validations.clone();
        for validation in actual_validations {
            if !FLAGS.use_ninja_validations {
                error_loc!(
                    n.lock().loc.as_ref(),
                    ".KATI_VALIDATIONS not allowed without --use_ninja_validations"
                );
            }
            let c = self.build_plan(validation, Some(output))?;
            n.lock().validations.push((validation, c));
        }

        // Block on werror_writable/werror_phony_looks_real, because otherwise we
        // can't rely on is_phony being valid for this check.
        if !n.lock().is_phony
            && n.lock().cmds.is_empty()
            && FLAGS.werror_writable
            && FLAGS.werror_phony_looks_real
        {
            let n = n.lock();
            if n.deps.is_empty() && n.order_onlys.is_empty() {
                if FLAGS.werror_real_no_cmds_or_deps {
                    error_loc!(
                        n.loc.as_ref(),
                        "*** target \"{output}\" has no commands or deps that could create it"
                    );
                } else if FLAGS.warn_real_no_cmds_or_deps {
                    warn_loc!(
                        n.loc.as_ref(),
                        "warning: target \"{output}\" has no commands or deps that could create it"
                    );
                }
            } else if n.actual_inputs.len() == 1 {
                if FLAGS.werror_real_no_cmds {
                    error_loc!(
                        n.loc.as_ref(),
                        "*** target \"{output}\" has no commands. Should \"{}\" be using .KATI_IMPLICIT_OUTPUTS?",
                        n.actual_inputs[0]
                    );
                } else if FLAGS.warn_real_no_cmds {
                    warn_loc!(
                        n.loc.as_ref(),
                        "warning: target \"{output}\" has no commands. Should \"{}\" be using .KATI_IMPLICIT_OUTPUTS?",
                        n.actual_inputs[0]
                    );
                }
            } else if FLAGS.werror_real_no_cmds {
                error_loc!(
                    n.loc.as_ref(),
                    "*** target \"{output}\" has no commands that could create output file. Is a dependency missing .KATI_IMPLICIT_OUTPUTS?"
                );
            } else if FLAGS.warn_real_no_cmds {
                warn_loc!(
                    n.loc.as_ref(),
                    "warning: target \"{output}\" has no commands that could create output file. Is a dependency missing .KATI_IMPLICIT_OUTPUTS?"
                );
            }
        }

        {
            let mut n = n.lock();
            n.has_rule = true;
            n.is_default_target = self.first_rule == Some(output);
            if let Some(cur_rule_vars) = &self.cur_rule_vars {
                let v = Vars::new();
                v.merge_from(cur_rule_vars);
                n.rule_vars = Some(Arc::new(v));
            } else {
                n.rule_vars = None
            }
        }

        Ok(n)
    }
}

pub fn make_dep(ev: &mut Evaluator, targets: Vec<Symbol>) -> Result<Vec<NamedDepNode>> {
    let mut db = DepBuilder::new(ev)?;
    let _tr = ScopedTimeReporter::new("make dep (build)");
    db.build(targets)
}

// sarun: count `(`/`{` minus `)`/`}` outside quotes — used to detect
// SECONDEXPANSION tokens that the first-pass word-split tore apart.
fn paren_balance(s: &[u8]) -> i32 {
    let mut depth = 0i32;
    for &b in s {
        match b {
            b'(' | b'{' => depth += 1,
            b')' | b'}' => depth -= 1,
            _ => {}
        }
    }
    depth
}

pub fn is_special_target(output: &Symbol) -> bool {
    let s = output.as_bytes();
    s.starts_with(b".") && !s[1..].starts_with(b".")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_suffix_rule() {
        assert!(is_suffix_rule(&intern(".c.o")));
        assert!(!is_suffix_rule(&intern("foo")));
        assert!(!is_suffix_rule(&intern(".co")));
        assert!(!is_suffix_rule(&intern(".c.o.b")));
    }
}
