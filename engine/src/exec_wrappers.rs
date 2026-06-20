//! In-process exec-wrapper builtins for box brush shells.
//!
//! `env` (and its relatives) are not algorithms to port — they are *launcher
//! front-ends*. Each one parses a few options, mutates the LOGICAL launch state
//! the box brush shell already maintains (environment, working directory, and
//! later signal dispositions / niceness / session), and then hands the residual
//! `COMMAND [ARG]...` to the shell's own command dispatch. There is nothing of
//! uutils worth keeping here: the uutils `env` is ~all `execvp`/`setenv`
//! plumbing that we would delete wholesale, because in-process the launch state
//! is brush's, not libc's. So these are written fresh against brush's seams.
//!
//! ## The mechanism: clone the shell, mutate the clone, run through dispatch
//!
//! A brush subshell is `shell.clone()` — it carries the logical cwd, the logical
//! environment, traps, and open files, but is NOT a separate OS process (see
//! `commands::invoke_command_in_subshell_and_get_output`). `env FOO=bar cmd`
//! must not leak `FOO`/`-C`/etc. into the calling shell, so we clone, apply the
//! mutations to the clone, and run `COMMAND` on the clone via
//! `Shell::run_string` (the same primitive `eval` uses) — which goes through the
//! full builtin/function/external dispatch and returns the command's real exit
//! status. The clone is dropped afterward, so the mutations vanish exactly when
//! they should.
//!
//! The mutated logical state materializes onto a real child only at fork→exec,
//! inside `compose_std_command`: the cloned env becomes the child's `environ`,
//! the cloned cwd becomes its `current_dir`. For an in-process target (a brush
//! builtin like `find`/`xargs`/`echo`) there is nothing to materialize onto a
//! process — and that is correct: the builtin reads the same logical state
//! directly (`context.shell.working_dir()`, the exported env), so `env -C dir
//! find .` and `env FOO=bar printenv FOO` both work with no OS process at all.
//!
//! ## Quoting the residual argv
//!
//! brush has already word-expanded the builtin's argv by the time we see it, so
//! to run `COMMAND [ARG]...` back through `run_string` (which re-parses a script
//! string) we force-single-quote every piece. That suppresses a second round of
//! alias/glob/word-splitting while still allowing normal command lookup —
//! exactly the "run this argv as a command" semantics `env` wants.

use std::io::Write;

use brush_core::escape::{self, QuoteMode};
use brush_core::variables::{ShellValue, ShellVariable};
use brush_core::{ExecutionResult, builtins};
use clap::Parser;

/// `env`'s own error exit status: it could not run the requested command for a
/// reason of its own (bad option, unusable `-C` directory). Matches GNU env.
const ENV_FAILURE: u8 = 125;

/// A `NAME=VALUE` assignment token split into its parts, or `None` if the token
/// is not a valid assignment (and therefore begins the `COMMAND`).
fn parse_assignment(token: &str) -> Option<(String, String)> {
    let (name, value) = token.split_once('=')?;
    if brush_core::env::valid_variable_name(name) {
        Some((name.to_string(), value.to_string()))
    } else {
        None
    }
}

/// The parsed shape of an `env` invocation: the launch-state mutations to apply,
/// plus the residual command (empty ⇒ print the resulting environment).
struct EnvPlan {
    ignore_env: bool,
    unset: Vec<String>,
    chdir: Option<String>,
    null_terminate: bool,
    assignments: Vec<(String, String)>,
    command: Vec<String>,
}

impl EnvPlan {
    /// Parse `env`'s grammar by hand:
    /// `env [-i] [-u NAME]... [-C DIR] [-0] [--] [NAME=VALUE]... [COMMAND [ARG]...]`.
    ///
    /// clap can't model this (options stop at the first operand, then the
    /// command keeps its own flags verbatim), so the builtin collects the raw
    /// argv and we walk it: an option phase, then `NAME=VALUE` assignments, then
    /// everything else is the command and its arguments untouched.
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
        // Option phase.
        while i < args.len() {
            let arg = &args[i];
            if arg == "--" {
                i += 1;
                break;
            }
            // A bare "-" is GNU's shorthand for -i.
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
            // First non-option token: assignments/command begin here.
            break;
        }

        // Assignment phase: consume `NAME=VALUE` tokens until the first one that
        // is not a valid assignment — that token starts the command.
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

/// Shared core for the `env` family: apply the plan's launch-state mutations to
/// a freshly cloned subshell, then either print the resulting environment (no
/// command) or run the command through that subshell's dispatch.
async fn run_env_plan<SE: brush_core::extensions::ShellExtensions>(
    plan: EnvPlan,
    context: brush_core::commands::ExecutionContext<'_, SE>,
) -> Result<ExecutionResult, brush_core::error::Error> {
    // Clone the calling shell into a subshell: the mutations below land on the
    // clone and are discarded when it drops, so nothing leaks to the caller.
    let mut subshell = context.shell.clone();

    // -i / bare '-' : start from an empty *exported* environment. We unexport
    // rather than delete, so the shell's own internal variables keep working
    // in-process while a materialized child still sees an empty `environ`.
    if plan.ignore_env {
        let names: Vec<String> = subshell
            .env()
            .iter_exported()
            .map(|(k, _)| k.clone())
            .collect();
        for n in &names {
            // get_mut yields (scope, &mut var); we only need the variable.
            if let Some((_, var)) = subshell.env_mut().get_mut(n) {
                var.unexport();
            }
        }
    }

    // -u NAME : remove the variable entirely.
    for name in &plan.unset {
        let _ = subshell.env_mut().unset(name);
    }

    // NAME=VALUE : set and export, so both in-process builtins and any
    // materialized child observe it.
    for (name, value) in &plan.assignments {
        let mut var = ShellVariable::new(ShellValue::String(value.clone()));
        var.export();
        subshell.env_mut().set_global(name.clone(), var)?;
    }

    // -C DIR : change the subshell's logical working directory. Materializes as
    // the child's cwd at exec, and is read directly by in-process builtins.
    if let Some(dir) = &plan.chdir {
        if let Err(e) = subshell.set_working_dir(dir) {
            let mut err = context.stderr();
            let _ = writeln!(err, "env: cannot change directory to '{dir}': {e}");
            return Ok(ExecutionResult::new(ENV_FAILURE));
        }
    }

    // No command: print the resulting environment, one `NAME=VALUE` per line
    // (NUL-terminated with -0). Both borrows of `subshell` are immutable.
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

    // Run COMMAND through the subshell's full dispatch. The argv is already
    // word-expanded, so force-quote each piece to round-trip it through
    // `run_string` without a second expansion. Command lookup (builtin /
    // function / external) still happens normally.
    let script = plan
        .command
        .iter()
        .map(|a| escape::force_quote(a, QuoteMode::SingleQuote))
        .collect::<Vec<_>>()
        .join(" ");

    let source_info = subshell.call_stack().current_pos_as_source_info();
    subshell
        .run_string(script, &source_info, &context.params)
        .await
}

/// `env` — run a command in a modified environment (or print the environment).
#[derive(Parser)]
pub(crate) struct EnvCommand {
    /// All arguments, collected raw; `env`'s grammar is parsed by hand because
    /// option processing must stop at the command and leave its flags intact.
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

/// `printenv` — print all (or named) environment variables.
///
/// Shares nothing of `env`'s command-running path: it only ever reports the
/// shell's logical exported environment, so it reads it directly off the
/// (unmodified) calling shell. With names, prints each named variable's value;
/// exit status is 1 if any requested name is unset.
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

        // With names: print each value; missing any ⇒ exit 1 (GNU printenv).
        // Only *exported* variables count as the environment, so look the name
        // up among the exported set rather than all shell variables.
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
