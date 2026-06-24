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
    os::unix::ffi::{OsStrExt, OsStringExt},
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
    log,
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
    done: HashMap<Symbol, ExecStatus>,
    shell: Bytes,
    shellflag: &'static [u8],
    num_commands: u64,
}

impl<'a> Executor<'a> {
    fn new(ev: &'a mut Evaluator) -> Result<Self> {
        let shell = ev.get_shell()?;
        let shellflag = ev.get_shell_flag();
        Ok(Executor {
            ce: CommandEvaluator::new(ev)?,
            done: HashMap::new(),
            shell,
            shellflag,
            num_commands: 0,
        })
    }

    fn exec_node(
        &mut self,
        n: &Arc<Mutex<DepNode>>,
        needed_by: Option<&[u8]>,
    ) -> Result<ExecStatus> {
        let output = n.lock().output;
        let output_str = output.as_bytes();
        if let Some(found) = self.done.get(&output) {
            if found == &ExecStatus::Processing {
                warn!(
                    "Circular {} <- {} dependency dropped.",
                    String::from_utf8_lossy(needed_by.unwrap_or(b"(null)")),
                    output
                )
            }
            return Ok(*found);
        }
        let loc = n.lock().loc.clone();
        let _frame = self
            .ce
            .ev
            .enter(FrameType::Exec, output_str.clone(), loc.unwrap_or_default());

        self.done.insert(output, ExecStatus::Processing);
        let output_timestamp = get_timestamp(&output_str, &self.ce.ev.working_dir)?;
        let output_ts = ExecStatus::Timestamp(output_timestamp);

        log!(
            "ExecNode: {output} for {}",
            String::from_utf8_lossy(needed_by.unwrap_or(b"(null)"))
        );

        if !n.lock().has_rule && output_timestamp.is_none() && !n.lock().is_phony {
            if let Some(needed_by) = needed_by {
                error!(
                    "*** No rule to make target '{output}', needed by '{}'.",
                    String::from_utf8_lossy(needed_by)
                );
            } else {
                error!("*** No rule to make target '{output}'");
            }
        }

        let mut latest = ExecStatus::Processing;
        let order_onlys = n.lock().order_onlys.clone();
        for (_, d) in order_onlys {
            let dep_out = d.lock().output.as_bytes();
            if std::fs::exists(OsStr::from_bytes(&dep_out))? {
                continue;
            }
            let ts = self.exec_node(&d, Some(&output_str))?;
            if latest < ts {
                latest = ts;
            }
        }

        let deps = n.lock().deps.clone();
        for (_, d) in deps {
            let ts = self.exec_node(&d, Some(&output_str))?;
            if latest < ts {
                latest = ts;
            }
        }

        if output_ts >= latest && !n.lock().is_phony {
            self.done.insert(output, output_ts);
            return Ok(output_ts);
        }

        // sarun: target-specific exported vars (`target: export VAR := …`)
        // — push them into the process env for the duration of THIS
        // target's commands, restore after. Single-threaded so the
        // env-swap is safe.
        let mut env_restores: Vec<(std::ffi::OsString, Option<std::ffi::OsString>)> = Vec::new();
        if let Some(rule_vars) = n.lock().rule_vars.clone() {
            let entries: Vec<(crate::symtab::Symbol, crate::var::Var)> =
                rule_vars.0.lock().iter().map(|(s, v)| (*s, v.clone())).collect();
            for (sym, var) in entries {
                let do_export = var.read().exported;
                let key = std::ffi::OsString::from_vec(sym.as_bytes().to_vec());
                if !do_export {
                    continue;
                }
                let value_bytes = var.read().eval_to_buf(self.ce.ev)?;
                let prev = std::env::var_os(&key);
                env_restores.push((key.clone(), prev));
                // SAFETY: single-threaded recipe loop.
                unsafe {
                    std::env::set_var(
                        &key,
                        <std::ffi::OsStr as OsStrExt>::from_bytes(&value_bytes),
                    );
                }
            }
        }

        let mut commands = self.ce.eval(n)?;
        // sarun: .ONESHELL — fuse all commands into one shell invocation
        // so variable/cwd state persists across recipe lines. The first
        // command's flags (echo, ignore_error) apply to the whole block.
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
        for command in commands {
            self.num_commands += 1;
            if command.echo {
                println!("{}", String::from_utf8_lossy(&command.cmd));
            }
            if !FLAGS.is_dry_run {
                // sarun: prefer the embedder's in-process runner (brush)
                // when installed; fall back to fork+exec /bin/sh otherwise.
                // The installed runner returns only an exit code, not a
                // signal-bearing ExitStatus — fine because the box's brush
                // path has no SIGINT/SIGQUIT signaling distinct from a
                // non-zero code.
                let (ok, output, code_for_msg) =
                    if let Some((code, out)) =
                        run_with_installed_runner(&self.shell, self.shellflag, &command.cmd)
                    {
                        (code == 0, out, code)
                    } else {
                        let (status, out) = run_command(
                            &self.shell,
                            self.shellflag,
                            &command.cmd,
                            RedirectStderr::Stdout,
                        )?;
                        (status.success(), out, status.code().unwrap_or(1))
                    };
                emit_recipe_output(&output);
                if !ok {
                    if command.ignore_error {
                        emit_recipe_err(&format!(
                            "[{}] Error {} (ignored)",
                            command.output, code_for_msg
                        ));
                    } else {
                        // sarun: .DELETE_ON_ERROR — remove the target's
                        // partially-created output file before bailing, and
                        // announce it the way GNU make does (the `*** ` prefix,
                        // after the Error line). Phony targets are never on-disk
                        // so skip them. The output path resolves against the
                        // Evaluator's working_dir (a -C sub-make's outputs live
                        // there, not at the process cwd).
                        emit_recipe_err(&format!(
                            "*** [{}] Error {}",
                            command.output, code_for_msg
                        ));
                        if self.ce.ev.delete_on_error && !n.lock().is_phony {
                            let out_bytes = command.output.as_bytes();
                            let rel = OsStr::from_bytes(&out_bytes);
                            let path = self.ce.ev.working_dir.join(rel);
                            if std::fs::exists(&path).unwrap_or(false) {
                                emit_recipe_err(&format!(
                                    "*** Deleting file \"{}\"",
                                    String::from_utf8_lossy(&out_bytes)
                                ));
                                let _ = std::fs::remove_file(&path);
                            }
                        }
                        // sarun: was std::process::exit(2) — that would kill the
                        // engine when running as an in-process builtin. Propagate
                        // instead; make_main/make_builtin return the code, and
                        // the standalone rkati main downcasts it to its exit.
                        return Err(BuildFailed(2).into());
                    }
                }
            }
        }

        for (key, prev) in env_restores.into_iter().rev() {
            // SAFETY: single-threaded.
            unsafe {
                match prev {
                    Some(v) => std::env::set_var(&key, v),
                    None => std::env::remove_var(&key),
                }
            }
        }

        self.done.insert(output, output_ts);
        Ok(output_ts)
    }
}

pub fn exec(roots: Vec<NamedDepNode>, ev: &mut Evaluator) -> Result<()> {
    let mut executor = Executor::new(ev)?;
    for (_sym, root) in &roots {
        executor.exec_node(root, None)?;
    }
    // sarun: emit "Nothing to be done" only for roots whose rule has no
    // commands at all (or which had no rule). If the rule had commands
    // but they were skipped because the file was up-to-date, GNU make
    // stays silent under -s (or prints "<target> is up to date"
    // otherwise); kati's old unconditional message diverged on every
    // benign incremental rebuild.
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
