//! In-process exec-wrapper builtins (`env`, `nice`, `setsid`, `nohup`).
//!
//! Each parses a few options, mutates the LOGICAL launch state brush maintains
//! (env, cwd, signal dispositions, niceness, session), then hands the residual
//! `COMMAND [ARG]...` to the shell's dispatch. Written fresh against brush's
//! seams — uutils' `env` is all `execvp`/`setenv` plumbing that would be deleted
//! wholesale; in-process, launch state is brush's, not libc's.
//!
//! ## Mechanism: clone, mutate, dispatch
//!
//! `shell.clone()` is a subshell: it carries the logical cwd/env/traps/open-files
//! but is NOT a separate OS process. We clone, apply mutations, and run the
//! residual argv via `Shell::run_argv` — full function/builtin/external dispatch
//! on the ALREADY-SPLIT argv (no re-expansion). The clone drops afterward so
//! mutations vanish exactly when they should.
//!
//! For a forked child, the mutated state materializes at fork→exec in
//! `compose_std_command` (cloned env → `environ`, cloned cwd → `current_dir`).
//! For an in-process builtin there is no process to materialize onto — correct:
//! the builtin reads logical state directly, so `env -C dir find .` and
//! `env FOO=bar printenv FOO` work with no OS process.

use std::io::Write;

use brush_core::openfiles::{OpenFile, OpenFiles};
use brush_core::variables::{ShellValue, ShellVariable};
use brush_core::{ExecutionParameters, ExecutionResult, Shell, builtins};
use clap::Parser;

/// `env`'s own error status (bad option, unusable `-C`). Matches GNU env.
const ENV_FAILURE: u8 = 125;

/// Split a `NAME=VALUE` token, or `None` if not a valid assignment (starts COMMAND).
fn parse_assignment(token: &str) -> Option<(String, String)> {
    let (name, value) = token.split_once('=')?;
    if brush_core::env::valid_variable_name(name) {
        Some((name.to_string(), value.to_string()))
    } else {
        None
    }
}

/// Parsed `env` invocation: launch-state mutations + residual command
/// (empty ⇒ print the resulting environment).
struct EnvPlan {
    ignore_env: bool,
    unset: Vec<String>,
    chdir: Option<String>,
    null_terminate: bool,
    assignments: Vec<(String, String)>,
    command: Vec<String>,
}

impl EnvPlan {
    /// Hand-parse `env [-i] [-u NAME]... [-C DIR] [-0] [--] [NAME=VALUE]... [COMMAND [ARG]...]`.
    /// clap can't model this (options stop at first operand; command keeps its own flags).
    /// Walks the argv: option phase → `NAME=VALUE` assignments → command verbatim.
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut plan = EnvPlan {
            ignore_env: false,
            unset: Vec::new(),
            chdir: None,
            null_terminate: false,
            assignments: Vec::new(),
            command: Vec::new(),
        };

        let mut i = 0;
        while i < args.len() { // option phase
            let arg = &args[i];
            if arg == "--" {
                i += 1;
                break;
            }
            // bare "-" is GNU shorthand for -i
            if arg == "-" {
                plan.ignore_env = true;
                i += 1;
                continue;
            }
            if let Some(long) = arg.strip_prefix("--") {
                let (name, inline) = match long.split_once('=') {
                    Some((n, v)) => (n, Some(v.to_string())),
                    None => (long, None),
                };
                match name {
                    "ignore-environment" => plan.ignore_env = true,
                    "null" => plan.null_terminate = true,
                    "unset" => {
                        let v = inline
                            .or_else(|| {
                                i += 1;
                                args.get(i).cloned()
                            })
                            .ok_or_else(|| "option '--unset' requires an argument".to_string())?;
                        plan.unset.push(v);
                    }
                    "chdir" => {
                        let v = inline
                            .or_else(|| {
                                i += 1;
                                args.get(i).cloned()
                            })
                            .ok_or_else(|| "option '--chdir' requires an argument".to_string())?;
                        plan.chdir = Some(v);
                    }
                    other => return Err(format!("unrecognized option '--{other}'")),
                }
                i += 1;
                continue;
            }
            if let Some(shorts) = arg.strip_prefix('-') {
                // Bundled short options, e.g. `-iu NAME` or `-uNAME` / `-CDIR`.
                let chars: Vec<char> = shorts.chars().collect();
                let mut j = 0;
                while j < chars.len() {
                    match chars[j] {
                        'i' => plan.ignore_env = true,
                        '0' => plan.null_terminate = true,
                        'u' | 'C' => {
                            let opt = chars[j];
                            // Value is the rest of this token, or the next arg.
                            let rest: String = chars[j + 1..].iter().collect();
                            let value = if rest.is_empty() {
                                i += 1;
                                args.get(i).cloned().ok_or_else(|| {
                                    format!("option requires an argument -- '{opt}'")
                                })?
                            } else {
                                rest
                            };
                            if opt == 'u' {
                                plan.unset.push(value);
                            } else {
                                plan.chdir = Some(value);
                            }
                            j = chars.len(); // value consumed the remainder
                            continue;
                        }
                        other => return Err(format!("invalid option -- '{other}'")),
                    }
                    j += 1;
                }
                i += 1;
                continue;
            }
            // first non-option: assignments/command begin
            break;
        }

        // Assignment phase: consume NAME=VALUE tokens until the first non-assignment.
        while i < args.len() {
            match parse_assignment(&args[i]) {
                Some(pair) => {
                    plan.assignments.push(pair);
                    i += 1;
                }
                None => break,
            }
        }

        plan.command = args[i..].to_vec();
        Ok(plan)
    }
}

/// Apply the plan's mutations to a cloned subshell, then print the environment
/// (no command) or dispatch the command through the subshell.
async fn run_env_plan<SE: brush_core::extensions::ShellExtensions>(
    plan: EnvPlan,
    context: brush_core::commands::ExecutionContext<'_, SE>,
) -> Result<ExecutionResult, brush_core::error::Error> {
    // Clone into a subshell: mutations land on the clone and vanish when it drops.
    let mut subshell = context.shell.clone();

    // -i / '-': empty the exported environment. Unexport (not delete) so
    // internal shell variables keep working; a child still sees an empty environ.
    if plan.ignore_env {
        let names: Vec<String> = subshell
            .env()
            .iter_exported()
            .map(|(k, _)| k.clone())
            .collect();
        for n in &names {
            // get_mut → (scope, &mut var); we only need the variable.
            if let Some((_, var)) = subshell.env_mut().get_mut(n) {
                var.unexport();
            }
        }
    }

    // -u NAME: remove the variable entirely.
    for name in &plan.unset {
        let _ = subshell.env_mut().unset(name);
    }

    // NAME=VALUE: set and export (visible to in-process builtins and forked children).
    for (name, value) in &plan.assignments {
        let mut var = ShellVariable::new(ShellValue::String(value.clone()));
        var.export();
        subshell.env_mut().set_global(name.clone(), var)?;
    }

    // -C DIR: change the subshell's logical cwd; materializes at exec for forked
    // children, read directly by in-process builtins.
    if let Some(dir) = &plan.chdir {
        if let Err(e) = subshell.set_working_dir(dir) {
            let mut err = context.stderr();
            let _ = writeln!(err, "env: cannot change directory to '{dir}': {e}");
            return Ok(ExecutionResult::new(ENV_FAILURE));
        }
    }

    // No command: print the resulting environment, one NAME=VALUE per line (-0 → NUL).
    if plan.command.is_empty() {
        let terminator = if plan.null_terminate { '\0' } else { '\n' };
        let mut out = context.stdout();
        for (name, var) in subshell.env().iter_exported() {
            let value = var.value().to_cow_str(&subshell);
            let _ = write!(out, "{name}={value}{terminator}");
        }
        let _ = out.flush();
        return Ok(ExecutionResult::success());
    }

    // Run the command through the subshell's full dispatch.
    dispatch(subshell, &plan.command, &context.params).await
}

/// Run an already-expanded `COMMAND [ARG]...` through a subshell via
/// `Shell::run_argv` (function/builtin/external dispatch, no re-expansion).
/// Shared tail for `env`, `nice`, `setsid`, `nohup`.
async fn dispatch<SE: brush_core::extensions::ShellExtensions>(
    mut subshell: Shell<SE>,
    command: &[String],
    params: &ExecutionParameters,
) -> Result<ExecutionResult, brush_core::error::Error> {
    subshell.run_argv(command, params).await
}

/// Split argv into leading option tokens (starting with `-`, up to `--`) and
/// the residual command. Shape shared by `nice`/`setsid`/`nohup` (unlike `env`,
/// which needs assignments between options and command).
fn split_options(args: &[String]) -> (Vec<String>, Vec<String>) {
    let mut opts = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            i += 1;
            break;
        }
        if a.starts_with('-') && a.len() > 1 {
            opts.push(a.clone());
            i += 1;
        } else {
            break;
        }
    }
    (opts, args[i..].to_vec())
}

/// `env` — run a command in a modified environment, or print the environment.
#[derive(Parser)]
pub(crate) struct EnvCommand {
    /// Raw argv; `env`'s grammar is parsed by hand (option stop at first operand;
    /// command keeps its own flags).
    #[clap(allow_hyphen_values = true, trailing_var_arg = true)]
    args: Vec<String>,
}

impl builtins::Command for EnvCommand {
    type Error = brush_core::error::Error;

    async fn execute<SE: brush_core::extensions::ShellExtensions>(
        &self,
        context: brush_core::commands::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        let plan = match EnvPlan::parse(&self.args) {
            Ok(plan) => plan,
            Err(msg) => {
                let mut err = context.stderr();
                let _ = writeln!(err, "env: {msg}");
                return Ok(ExecutionResult::new(ENV_FAILURE));
            }
        };
        run_env_plan(plan, context).await
    }
}

/// `printenv` — print all (or named) exported environment variables.
/// With names, exit 1 if any is unset (GNU printenv).
#[derive(Parser)]
pub(crate) struct PrintenvCommand {
    /// End each output line with NUL, not newline.
    #[clap(short = '0', long = "null")]
    null_terminate: bool,

    /// Variable names to print; if none, print the whole environment.
    #[clap(allow_hyphen_values = true)]
    names: Vec<String>,
}

impl builtins::Command for PrintenvCommand {
    type Error = brush_core::error::Error;

    async fn execute<SE: brush_core::extensions::ShellExtensions>(
        &self,
        context: brush_core::commands::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        let terminator = if self.null_terminate { '\0' } else { '\n' };
        let mut out = context.stdout();

        if self.names.is_empty() {
            for (name, var) in context.shell.env().iter_exported() {
                let value = var.value().to_cow_str(context.shell);
                let _ = write!(out, "{name}={value}{terminator}");
            }
            let _ = out.flush();
            return Ok(ExecutionResult::success());
        }

        // With names: print each value; missing any → exit 1 (GNU printenv).
        // Only exported variables count; look up in iter_exported, not all vars.
        let mut all_found = true;
        for name in &self.names {
            match context
                .shell
                .env()
                .iter_exported()
                .find(|(k, _)| k.as_str() == name)
            {
                Some((_, var)) => {
                    let value = var.value().to_cow_str(context.shell);
                    let _ = write!(out, "{value}{terminator}");
                }
                None => all_found = false,
            }
        }
        let _ = out.flush();
        Ok(ExecutionResult::new(u8::from(!all_found)))
    }
}

// ─── Launch-state wrappers: nice / setsid / nohup ────────────────────────────
// Each sets a `LaunchState` override on a cloned `ExecutionParameters` that
// materializes just before execve in `compose_std_command`. For in-process
// builtin targets there is no exec, so the wrapper is a transparent pass-through
// — correct (a builtin has no process to renice/session/SIGHUP-shield).

/// `nice` — run a command with an adjusted scheduling priority.
#[derive(Parser)]
pub(crate) struct NiceCommand {
    /// Raw argv; `-n N` takes a separate value and the command keeps its own flags.
    #[clap(allow_hyphen_values = true, trailing_var_arg = true)]
    args: Vec<String>,
}

impl builtins::Command for NiceCommand {
    type Error = brush_core::error::Error;

    async fn execute<SE: brush_core::extensions::ShellExtensions>(
        &self,
        context: brush_core::commands::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        // GNU default: +10 (lower priority).
        let mut adjustment: i32 = 10;
        let args = &self.args;
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            if arg == "--" {
                i += 1;
                break;
            }
            // --adjustment[=N] or --adjustment N
            if let Some(rest) = arg.strip_prefix("--adjustment") {
                let val = if let Some(v) = rest.strip_prefix('=') {
                    v.to_string()
                } else {
                    i += 1;
                    match args.get(i) {
                        Some(v) => v.clone(),
                        None => return nice_usage_error(&context, "option '--adjustment' requires an argument"),
                    }
                };
                match val.parse::<i32>() {
                    Ok(n) => adjustment = n,
                    Err(_) => return nice_usage_error(&context, &format!("invalid adjustment '{val}'")),
                }
                i += 1;
                continue;
            }
            // -n N or -nN
            if arg == "-n" {
                i += 1;
                let val = match args.get(i) {
                    Some(v) => v.clone(),
                    None => return nice_usage_error(&context, "option requires an argument -- 'n'"),
                };
                match val.parse::<i32>() {
                    Ok(n) => adjustment = n,
                    Err(_) => return nice_usage_error(&context, &format!("invalid adjustment '{val}'")),
                }
                i += 1;
                continue;
            }
            if let Some(v) = arg.strip_prefix("-n") {
                match v.parse::<i32>() {
                    Ok(n) => adjustment = n,
                    Err(_) => return nice_usage_error(&context, &format!("invalid adjustment '{v}'")),
                }
                i += 1;
                continue;
            }
            // Historical: `-NUM` / `--NUM` is an adjustment of NUM.
            if let Some(body) = arg.strip_prefix('-') {
                if let Ok(n) = body.trim_start_matches('-').parse::<i32>() {
                    adjustment = n;
                    i += 1;
                    continue;
                }
                return nice_usage_error(&context, &format!("invalid option '{arg}'"));
            }
            break;
        }

        let command = args[i..].to_vec();

        // No command: print current niceness (GNU behavior).
        if command.is_empty() {
            let cur = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
            let mut out = context.stdout();
            let _ = writeln!(out, "{cur}");
            let _ = out.flush();
            return Ok(ExecutionResult::success());
        }

        // Target = inherited niceness + adjustment, clamped to [-20, 19].
        // Raising priority may be denied in the child; the command still runs.
        let base = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
        let target = (base + adjustment).clamp(-20, 19);

        let subshell = context.shell.clone();
        let mut params = context.params.clone();
        params.launch_state.niceness = Some(target);
        dispatch(subshell, &command, &params).await
    }
}

/// Emit a `nice:`-prefixed diagnostic and return GNU nice's error status (125).
fn nice_usage_error<SE: brush_core::extensions::ShellExtensions>(
    context: &brush_core::commands::ExecutionContext<'_, SE>,
    msg: &str,
) -> Result<ExecutionResult, brush_core::error::Error> {
    let mut err = context.stderr();
    let _ = writeln!(err, "nice: {msg}");
    Ok(ExecutionResult::new(125))
}

/// `setsid` — run a command in a new session.
#[derive(Parser)]
pub(crate) struct SetsidCommand {
    #[clap(allow_hyphen_values = true, trailing_var_arg = true)]
    args: Vec<String>,
}

impl builtins::Command for SetsidCommand {
    type Error = brush_core::error::Error;

    async fn execute<SE: brush_core::extensions::ShellExtensions>(
        &self,
        context: brush_core::commands::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        let (opts, command) = split_options(&self.args);
        // run_argv always waits, so -w/--wait is a no-op; -c/--ctty and -f/--fork
        // are accepted but not modeled.
        for o in &opts {
            match o.as_str() {
                "-w" | "--wait" | "-c" | "--ctty" | "-f" | "--fork" => {}
                other => {
                    let mut err = context.stderr();
                    let _ = writeln!(err, "setsid: invalid option '{other}'");
                    return Ok(ExecutionResult::new(1));
                }
            }
        }
        if command.is_empty() {
            let mut err = context.stderr();
            let _ = writeln!(err, "setsid: no command specified");
            return Ok(ExecutionResult::new(1));
        }

        let subshell = context.shell.clone();
        let mut params = context.params.clone();
        params.launch_state.new_session = true;
        dispatch(subshell, &command, &params).await
    }
}

/// `nohup` — run a command immune to hangups.
#[derive(Parser)]
pub(crate) struct NohupCommand {
    #[clap(allow_hyphen_values = true, trailing_var_arg = true)]
    args: Vec<String>,
}

impl builtins::Command for NohupCommand {
    type Error = brush_core::error::Error;

    async fn execute<SE: brush_core::extensions::ShellExtensions>(
        &self,
        context: brush_core::commands::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        let (opts, command) = split_options(&self.args);
        for o in &opts {
            // nohup has no options of its own beyond --help/--version.
            let mut err = context.stderr();
            let _ = writeln!(err, "nohup: invalid option '{o}'");
            return Ok(ExecutionResult::new(125));
        }
        if command.is_empty() {
            let mut err = context.stderr();
            let _ = writeln!(err, "nohup: missing operand");
            return Ok(ExecutionResult::new(125));
        }

        let subshell = context.shell.clone();
        let mut params = context.params.clone();
        params.launch_state.ignore_sighup = true;
        // Redirect stdin from /dev/null so the detached command can't steal box stdin.
        // stdout→nohup.out is intentionally omitted: a box's stdout is always a
        // pipe/file (never a terminal), which is exactly when GNU nohup leaves it.
        if let Ok(devnull) = std::fs::File::open("/dev/null") {
            params.set_fd(OpenFiles::STDIN_FD, OpenFile::from(devnull));
        }
        dispatch(subshell, &command, &params).await
    }
}
