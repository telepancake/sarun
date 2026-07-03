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
    env,
    ffi::{OsStr, OsString},
    os::unix::ffi::{OsStrExt, OsStringExt},
    sync::LazyLock,
    vec::IntoIter,
};

use crate::{
    strutil::{Pattern, word_scanner},
    symtab::intern,
};
use bytes::Bytes;
use parking_lot::Mutex;

// sarun: an explicit args override. When set (before FLAGS is first read), the
// global FLAGS is built from THESE args instead of env::args_os(). sarun's
// embedded `make` path uses this to drive kati in ninja-generation mode against
// a synthesized argv (--ninja injected, makefile/targets/VAR=val translated
// from the box's make argv) without having to be the real process argv0.
pub static FLAGS_ARGS_OVERRIDE: std::sync::OnceLock<Vec<OsString>> = std::sync::OnceLock::new();

/// sarun: install the args FLAGS should parse. Must be called BEFORE the first
/// access to FLAGS (it is a LazyLock; the first deref freezes the value). Returns
/// Err(()) if the override was already set.
pub fn install_args(args: Vec<OsString>) -> Result<(), ()> {
    FLAGS_ARGS_OVERRIDE.set(args).map_err(|_| ())
}

pub static FLAGS: LazyLock<Flags> = LazyLock::new(|| {
    if let Some(args) = FLAGS_ARGS_OVERRIDE.get() {
        // sarun: explicit override wins (see install_args).
        Flags::from_args(args.clone())
    } else if cfg!(test) {
        Flags::default()
    } else {
        Flags::from_args(env::args_os().collect())
    }
});

#[derive(Default)]
pub struct Flags {
    pub detect_android_echo: bool,
    pub detect_depfiles: bool,
    pub dump_kati_stamp: bool,
    pub dump_include_graph: Option<OsString>,
    pub dump_variable_assignment_trace: Option<OsString>,
    pub enable_debug: bool,
    pub enable_kati_warnings: bool,
    pub enable_stat_logs: bool,
    pub gen_all_targets: bool,
    pub generate_ninja: bool,
    pub generate_empty_ninja: bool,
    pub is_dry_run: bool,
    pub is_silent_mode: bool,
    /// -k / --keep-going: on a recipe failure, keep building targets that
    /// don't depend on the failed one; exit non-zero at the end.
    pub is_keep_going: bool,
    pub is_syntax_check_only: bool,
    pub regen: bool,
    pub regen_debug: bool,
    pub regen_ignoring_kati_binary: bool,
    pub use_find_emulator: bool,
    pub color_warnings: bool,
    pub no_builtin_rules: bool,
    pub no_builtin_variables: bool,
    pub no_ninja_prelude: bool,
    pub use_ninja_phony_output: bool,
    pub use_ninja_validations: bool,
    pub emit_sandbox_disabled: bool,
    pub werror_find_emulator: bool,
    pub werror_overriding_commands: bool,
    pub warn_implicit_rules: bool,
    pub werror_implicit_rules: bool,
    pub warn_suffix_rules: bool,
    pub werror_suffix_rules: bool,
    pub top_level_phony: bool,
    pub warn_real_to_phony: bool,
    pub werror_real_to_phony: bool,
    pub warn_phony_looks_real: bool,
    pub werror_phony_looks_real: bool,
    pub werror_writable: bool,
    pub warn_real_no_cmds_or_deps: bool,
    pub werror_real_no_cmds_or_deps: bool,
    pub warn_real_no_cmds: bool,
    pub werror_real_no_cmds: bool,
    pub default_pool: OsString,
    pub ignore_dirty_pattern: Option<crate::strutil::Pattern>,
    pub no_ignore_dirty_pattern: Option<crate::strutil::Pattern>,
    pub ignore_optional_include_pattern: Option<crate::strutil::Pattern>,
    pub makefile: Mutex<Option<OsString>>,
    pub ninja_dir: Option<OsString>,
    pub ninja_suffix: OsString,
    pub working_dir: Option<OsString>, // -C <dir>
    pub include_dirs: Vec<OsString>,   // -I / --include-dir
    pub num_cpus: usize,
    pub num_jobs: usize,
    // sarun: true when -j was given explicitly on the command line (vs the
    // num_jobs default of CPU count). The parallel executor only fans out when
    // -j>1 was actually requested — so a plain `make` (and the serial corpus)
    // stays serial, exactly like GNU make.
    pub jobs_explicit: bool,
    pub remote_num_jobs: usize,
    pub subkati_args: Vec<OsString>,
    pub targets: Vec<crate::symtab::Symbol>,
    pub cl_vars: Vec<Bytes>,
    pub writable: Vec<OsString>,
    pub traced_variables_pattern: Vec<crate::strutil::Pattern>,

    pub cpu_profile_path: Option<OsString>,
    pub memory_profile_path: Option<OsString>,
}

fn parse_command_line_option_with_arg(
    option: &str,
    arg: &OsStr,
    args: &mut IntoIter<OsString>,
) -> Option<OsString> {
    let arg = arg.as_bytes();
    let arg = arg.strip_prefix(option.as_bytes())?;
    if arg.is_empty() {
        return args.next();
    }
    if let Some(arg) = arg.strip_prefix(b"=") {
        return Some(OsString::from_vec(arg.to_vec()));
    }
    // E.g, -j999
    if option.len() == 2 {
        return Some(OsString::from_vec(arg.to_vec()));
    }
    None
}

impl Flags {
    /// sarun: pub so an embedder can parse a per-instance Flags from a
    /// synthesized argv instead of relying solely on the install-once global
    /// FLAGS — needed when multiple make invocations share one process (the
    /// in-process `make` builtin). The global FLAGS still supplies the
    /// immutable mode-switches; per-instance INPUTS (makefile/targets/cl_vars/
    /// working_dir) come from a local Flags so concurrent makes don't collide.
    /// sarun: fold an inherited MAKEFLAGS value into this Flags — GNU make's
    /// env-to-sub-make flag channel (--include-dir, -rR/-s letter words,
    /// VAR=val overrides). from_args applies the PROCESS env's MAKEFLAGS; an
    /// embedder whose makes carry their environment out-of-band (the
    /// in-process `make` builtin's seed_env — many makes share one process
    /// env) calls this again with the make's OWN inherited value, which is
    /// where e.g. the Linux kernel's `MAKEFLAGS += --include-dir=$(abs_srctree)`
    /// actually arrives.
    /// GNU-style quoting for a variable definition carried in MAKEFLAGS'
    /// `--` section: backslash-escape whitespace and backslash, double `$`.
    pub fn quote_for_makeflags(v: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(v.len() + 8);
        for &b in v {
            match b {
                b' ' | b'\t' | b'\\' => {
                    out.push(b'\\');
                    out.push(b);
                }
                b'$' => {
                    out.push(b'$');
                    out.push(b'$');
                }
                _ => out.push(b),
            }
        }
        out
    }

    /// Split a MAKEFLAGS value into (flag words, `--`-section variable
    /// definitions). Variable definitions are unescaped (the inverse of
    /// quote_for_makeflags); the split on the separator and on definition
    /// boundaries honors backslash escapes.
    pub fn split_makeflags(makeflags: &[u8]) -> (Vec<Bytes>, Vec<Bytes>) {
        let mut flags: Vec<Bytes> = vec![];
        let mut vars: Vec<Bytes> = vec![];
        let mut in_vars = false;
        let mut cur: Vec<u8> = vec![];
        let mut i = 0;
        while i <= makeflags.len() {
            let b = makeflags.get(i).copied();
            match b {
                Some(b'\\') if in_vars && i + 1 < makeflags.len() => {
                    cur.push(makeflags[i + 1]);
                    i += 2;
                    continue;
                }
                Some(b'$') if in_vars && makeflags.get(i + 1) == Some(&b'$') => {
                    cur.push(b'$');
                    i += 2;
                    continue;
                }
                Some(b' ') | Some(b'\t') | None => {
                    if !cur.is_empty() {
                        if !in_vars && cur == b"--" {
                            in_vars = true;
                        } else if in_vars && cur.contains(&b'=') {
                            vars.push(Bytes::from(std::mem::take(&mut cur)));
                        } else {
                            // A word without '=' is a SWITCH wherever it
                            // appears — a makefile-level `MAKEFLAGS += --foo`
                            // textually lands after the `--` section, and
                            // GNU still treats it as a flag.
                            flags.push(Bytes::from(std::mem::take(&mut cur)));
                        }
                        cur.clear();
                    }
                    if b.is_none() {
                        break;
                    }
                }
                Some(c) => cur.push(c),
            }
            i += 1;
        }
        (flags, vars)
    }

    /// True if a MAKEFLAGS value carries -k / --keep-going (long, short,
    /// or a bare GNU letter-word like "ks").
    pub fn makeflags_keep_going(makeflags: &[u8]) -> bool {
        let (words, _) = Flags::split_makeflags(makeflags);
        words.iter().any(|w| {
            w[..] == *b"-k"
                || w[..] == *b"--keep-going"
                || (!w.starts_with(b"-") && !w.contains(&b'=')
                    && w.iter().all(|&b| b.is_ascii_alphabetic())
                    && w.contains(&b'k'))
        })
    }

    pub fn apply_makeflags(&mut self, makeflags: &[u8]) {
        // GNU carries command-line variable overrides after a `--`
        // separator, space-escaped. Fold them into cl_vars and keep only
        // the flag words for the token loop below.
        let (flag_words, var_defs) = Flags::split_makeflags(makeflags);
        for var in var_defs {
            if !self.cl_vars.contains(&var) {
                self.cl_vars.push(var);
            }
        }
        for tok in flag_words.iter().map(|f| &f[..]) {
            if let Some(dir) = tok.strip_prefix(b"--include-dir=") {
                let dir = OsString::from_vec(dir.to_vec());
                if !self.include_dirs.contains(&dir) {
                    self.include_dirs.push(dir);
                }
            } else if tok == b"--no-builtin-rules" || tok == b"--no_builtin_rules" {
                self.no_builtin_rules = true;
            } else if tok == b"--no-builtin-variables" || tok == b"--no_builtin_variables" {
                self.no_builtin_variables = true;
            } else if tok == b"-s" || tok == b"--silent" || tok == b"--quiet" {
                self.is_silent_mode = true;
            } else if tok == b"-k" || tok == b"--keep-going" {
                self.is_keep_going = true;
            } else if !tok.starts_with(b"-") && tok.contains(&b'=') {
                let var = Bytes::from(tok.to_vec());
                if !self.cl_vars.contains(&var) {
                    self.cl_vars.push(var);
                }
            } else if !tok.starts_with(b"-") && !tok.contains(&b'=') && tok.iter().all(|&b| b.is_ascii_alphabetic()) {
                // GNU make encodes short flags as a bare letter-word in
                // MAKEFLAGS (e.g. "rRs" for -r -R -s). Parse the
                // semantically important ones.
                for &ch in tok {
                    match ch {
                        b'r' => self.no_builtin_rules = true,
                        b'R' => self.no_builtin_variables = true,
                        b's' => self.is_silent_mode = true,
                        b'k' => self.is_keep_going = true,
                        _ => {}
                    }
                }
            }
        }
    }

    pub fn from_args(args: Vec<OsString>) -> Flags {
        let mut iter = args.into_iter();
        let mut flags = Flags::default();
        flags.subkati_args.push(iter.next().unwrap());
        flags.num_jobs = std::thread::available_parallelism().map_or(1, |p| p.get());
        flags.num_cpus = flags.num_jobs;

        if let Some(makeflags) = env::var_os("MAKEFLAGS") {
            flags.apply_makeflags(makeflags.as_bytes());
        }

        while let Some(arg) = iter.next() {
            let mut should_propagate = true;
            match arg.as_bytes() {
                b"-f" => {
                    *flags.makefile.lock() = iter.next();
                    should_propagate = false;
                }
                b"-c" => flags.is_syntax_check_only = true,
                b"-i" => flags.is_dry_run = true,
                b"-s" => flags.is_silent_mode = true,
                b"-r" => flags.no_builtin_rules = true,
                b"-R" => flags.no_builtin_variables = true,
                b"-w" | b"-n" => {} // accepted, semantically no-op in kati
                b"-k" | b"--keep-going" => flags.is_keep_going = true,
                // sarun: GNU make's long forms of -w/its inverse. The kernel's
                // top Makefile passes --no-print-directory to every sub-make
                // (see the __sub-make dance around MAKEFLAGS); without an
                // accepted arm here kati's catch-all below panics with
                // "Unknown flag" on the very first recursive invocation.
                b"--print-directory" | b"--no-print-directory" => {}
                b"-d" => flags.enable_debug = true,
                b"--kati_stats" => flags.enable_stat_logs = true,
                b"--warn" => flags.enable_kati_warnings = true,
                b"--ninja" => flags.generate_ninja = true,
                b"--empty_ninja_file" => flags.generate_empty_ninja = true,
                b"--gen_all_targets" => flags.gen_all_targets = true,
                b"--regen" => {
                    // TODO: Make this default.
                    flags.regen = true
                }
                b"--regen_debug" => flags.regen_debug = true,
                b"--regen_ignoring_kati_binary" => flags.regen_ignoring_kati_binary = true,
                b"--dump_kati_stamp" => {
                    flags.dump_kati_stamp = true;
                    flags.regen_debug = true;
                }
                b"--detect_android_echo" => flags.detect_android_echo = true,
                b"--detect_depfiles" => flags.detect_depfiles = true,
                b"--color_warnings" => flags.color_warnings = true,
                b"--no_builtin_rules" => flags.no_builtin_rules = true,
                b"--no_ninja_prelude" => flags.no_ninja_prelude = true,
                b"--use_ninja_phony_output" => flags.use_ninja_phony_output = true,
                b"--use_ninja_validations" => flags.use_ninja_validations = true,
                b"--emit_sandbox_disabled" => flags.emit_sandbox_disabled = true,
                b"--werror_find_emulator" => flags.werror_find_emulator = true,
                b"--werror_overriding_commands" => flags.werror_overriding_commands = true,
                b"--warn_implicit_rules" => flags.warn_implicit_rules = true,
                b"--werror_implicit_rules" => flags.werror_implicit_rules = true,
                b"--warn_suffix_rules" => flags.warn_suffix_rules = true,
                b"--werror_suffix_rules" => flags.werror_suffix_rules = true,
                b"--top_level_phony" => flags.top_level_phony = true,
                b"--warn_real_to_phony" => flags.warn_real_to_phony = true,
                b"--werror_real_to_phony" => {
                    flags.warn_real_to_phony = true;
                    flags.werror_real_to_phony = true;
                }
                b"--warn_phony_looks_real" => flags.warn_phony_looks_real = true,
                b"--werror_phony_looks_real" => {
                    flags.warn_phony_looks_real = true;
                    flags.werror_phony_looks_real = true;
                }
                b"--werror_writable" => flags.werror_writable = true,
                b"--warn_real_no_cmds_or_deps" => flags.warn_real_no_cmds_or_deps = true,
                b"--werror_real_no_cmds_or_deps" => {
                    flags.warn_real_no_cmds_or_deps = true;
                    flags.werror_real_no_cmds_or_deps = true;
                }
                b"--warn_real_no_cmds" => flags.warn_real_no_cmds = true,
                b"--werror_real_no_cmds" => {
                    flags.warn_real_no_cmds = true;
                    flags.werror_real_no_cmds = true;
                }
                b"--use_find_emulator" => flags.use_find_emulator = true,
                _ => {
                    if let Some(arg) = parse_command_line_option_with_arg("-C", &arg, &mut iter) {
                        flags.working_dir = Some(arg);
                    } else if let Some(arg) = parse_command_line_option_with_arg("-I", &arg, &mut iter) {
                        flags.include_dirs.push(arg);
                    } else if let Some(arg) = parse_command_line_option_with_arg("--include-dir", &arg, &mut iter) {
                        flags.include_dirs.push(arg);
                    } else if let Some(arg) =
                        parse_command_line_option_with_arg("--dump_include_graph", &arg, &mut iter)
                    {
                        flags.dump_include_graph = Some(arg);
                    } else if let Some(arg) = parse_command_line_option_with_arg(
                        "--dump_variable_assignment_trace",
                        &arg,
                        &mut iter,
                    ) {
                        flags.dump_variable_assignment_trace = Some(arg);
                    } else if let Some(arg) = parse_command_line_option_with_arg(
                        "--variable_assignment_trace_filter",
                        &arg,
                        &mut iter,
                    ) {
                        for pat in word_scanner(arg.as_bytes()) {
                            flags
                                .traced_variables_pattern
                                .push(Pattern::new(Bytes::from(pat.to_vec())));
                        }
                    } else if let Some(arg) =
                        parse_command_line_option_with_arg("-j", &arg, &mut iter)
                    {
                        let Some(num_jobs) = arg.to_string_lossy().parse::<usize>().ok() else {
                            panic!("Invalid -j flag: {}", arg.to_string_lossy());
                        };
                        flags.num_jobs = num_jobs;
                        flags.jobs_explicit = true;
                        // sarun: -j does NOT belong in $(MAKE) — GNU make keeps the
                        // job count out of the MAKE variable (it rides MAKEFLAGS /
                        // the jobserver instead). subkati_args feeds $(MAKE), so
                        // don't propagate -j into it, or a `$(MAKE) …` recipe would
                        // echo `make -jN …` and diverge from make.
                        should_propagate = false;
                    } else if let Some(arg) =
                        parse_command_line_option_with_arg("--remote_num_jobs", &arg, &mut iter)
                    {
                        let Some(num_jobs) = arg.to_string_lossy().parse::<usize>().ok() else {
                            panic!("Invalid --remote_num_jobs flag: {}", arg.to_string_lossy());
                        };
                        flags.remote_num_jobs = num_jobs;
                    } else if let Some(arg) =
                        parse_command_line_option_with_arg("--ninja_suffix", &arg, &mut iter)
                    {
                        flags.ninja_suffix = arg;
                    } else if let Some(arg) =
                        parse_command_line_option_with_arg("--ninja_dir", &arg, &mut iter)
                    {
                        flags.ninja_dir = Some(arg);
                    } else if let Some(arg) = parse_command_line_option_with_arg(
                        "--ignore_optional_include",
                        &arg,
                        &mut iter,
                    ) {
                        flags.ignore_optional_include_pattern =
                            Some(Pattern::new(Bytes::from(arg.as_bytes().to_vec())));
                    } else if let Some(arg) =
                        parse_command_line_option_with_arg("--ignore_dirty", &arg, &mut iter)
                    {
                        flags.ignore_dirty_pattern =
                            Some(Pattern::new(Bytes::from(arg.as_bytes().to_vec())));
                    } else if let Some(arg) =
                        parse_command_line_option_with_arg("--no_ignore_dirty", &arg, &mut iter)
                    {
                        flags.no_ignore_dirty_pattern =
                            Some(Pattern::new(Bytes::from(arg.as_bytes().to_vec())));
                    } else if let Some(arg) =
                        parse_command_line_option_with_arg("--writable", &arg, &mut iter)
                    {
                        flags.writable.push(arg);
                    } else if let Some(arg) =
                        parse_command_line_option_with_arg("--default_pool", &arg, &mut iter)
                    {
                        flags.default_pool = arg;
                    } else if let Some(arg) =
                        parse_command_line_option_with_arg("--cpu_profile", &arg, &mut iter)
                    {
                        flags.cpu_profile_path = Some(arg)
                    } else if let Some(arg) =
                        parse_command_line_option_with_arg("--mem_profile", &arg, &mut iter)
                    {
                        flags.memory_profile_path = Some(arg)
                    } else if arg.as_bytes().starts_with(b"-") {
                        panic!("Unknown flag: {}", arg.to_string_lossy());
                    } else if arg.as_bytes().contains(&b'=') {
                        flags.cl_vars.push(Bytes::from(arg.as_bytes().to_vec()));
                    } else {
                        should_propagate = false;
                        let arg = Bytes::from(arg.as_bytes().to_vec());
                        flags.targets.push(intern(arg));
                    }
                }
            }
            if should_propagate {
                flags.subkati_args.push(arg);
            }
        }

        if !flags.traced_variables_pattern.is_empty()
            && flags.dump_variable_assignment_trace.is_none()
        {
            panic!(
                "--variable_assignment_trace_filter is valid only together with --dump_variable_assignment_trace"
            );
        }

        flags
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_flags() {
        let flags = Flags::from_args(
            vec!["test", "-f", "main.mk"]
                .into_iter()
                .map(|s| s.into())
                .collect(),
        );
        assert_eq!(flags.makefile.lock().clone().unwrap(), "main.mk");
    }

    #[test]
    fn test_parse_command_line_option_with_arg() {
        assert_eq!(
            parse_command_line_option_with_arg(
                "--ignore_optional_include",
                &OsString::from("--ignore_optional_include=out/%.P"),
                &mut vec![].into_iter()
            ),
            Some(OsString::from("out/%.P"))
        );
    }
}
