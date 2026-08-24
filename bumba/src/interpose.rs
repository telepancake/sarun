//! In-process replacement for pathname invocations Bumba owns.
//!
//! Shell discovery must continue to report real executables: build systems use
//! `command -v`, `type -P`, and configure probes to distinguish ordinary shell
//! builtins from external utilities.  Once one of those paths is invoked,
//! however, Bumba must route it back to the same in-process implementation as
//! the bare command name.  The same boundary keeps nested `sh`/`bash` scripts
//! inside Brush, including Autoconf's early `exec $CONFIG_SHELL ./configure`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use brush_core::commands::ExecInterposer;
use brush_core::{ExecutionParameters, ExecutionResult, Shell, SourceInfo};

const SHELL_NAMES: &[&str] = &["sh", "dash", "bash"];

const EXTERNAL_BUILTINS: &[&str] = &[
    "cat", "head", "tail", "wc", "nl", "tac", "basename", "dirname", "seq",
    "expr", "tr", "cut", "uniq", "sort", "uname", "nproc", "id", "whoami",
    "cp", "mkdir", "rmdir", "rm", "mv", "ln", "touch", "readlink", "realpath",
    "mktemp", "tee", "chmod", "chown", "install", "find", "xargs", "env",
    "printenv", "nice", "setsid", "nohup", "make", "gmake", "ninja",
];

#[derive(Clone, Debug)]
enum Candidate {
    Builtin(String),
    Shell {
        bash_mode: bool,
        script: Option<PathBuf>,
    },
}

#[derive(Debug)]
struct ShellInvocation {
    source: ScriptSource,
    dollar0: String,
    positional: Vec<String>,
    set_flags: Vec<String>,
    bash_mode: bool,
}

#[derive(Debug)]
enum ScriptSource {
    Literal(String),
    Path(PathBuf),
}

const SET_FLAGS: &str = "euxvfnhmbCa";
static SYNTHETIC_PID: AtomicU32 = AtomicU32::new(0x5000_0000);

pub(crate) fn install(shell: &mut Shell) {
    shell.set_exec_interposer(std::sync::Arc::new(BumbaInterposer));
}

fn basename(path: &Path) -> Option<&str> {
    path.file_name()?.to_str()
}

fn shell_mode(name: &str) -> Option<bool> {
    SHELL_NAMES.contains(&name).then_some(name == "bash")
}

fn shebang_shell(path: &Path) -> Option<bool> {
    use std::io::Read as _;

    let mut file = std::fs::File::open(path).ok()?;
    let mut head = [0u8; 512];
    let count = file.read(&mut head).ok()?;
    let line = head[..count].strip_prefix(b"#!")?;
    let end = line.iter().position(|byte| *byte == b'\n').unwrap_or(line.len());
    let words = std::str::from_utf8(&line[..end])
        .ok()?
        .split_whitespace()
        .collect::<Vec<_>>();
    let interpreter = words.first().and_then(|word| basename(Path::new(word)))?;
    if interpreter == "env" {
        words
            .iter()
            .skip(1)
            .find(|word| !word.starts_with('-'))
            .and_then(|word| shell_mode(basename(Path::new(word)).unwrap_or(word)))
    } else {
        shell_mode(interpreter)
    }
}

fn classify(path: &Path) -> Option<Candidate> {
    if let Some(name) = basename(path) {
        if let Some(bash_mode) = shell_mode(name) {
            return Some(Candidate::Shell {
                bash_mode,
                script: None,
            });
        }
        if EXTERNAL_BUILTINS.contains(&name) {
            return Some(Candidate::Builtin(name.to_owned()));
        }
    }
    shebang_shell(path).map(|bash_mode| Candidate::Shell {
        bash_mode,
        script: Some(path.to_path_buf()),
    })
}

fn parse_shell_invocation(
    candidate: &Candidate,
    argv: &[String],
) -> Result<ShellInvocation, String> {
    let Candidate::Shell { bash_mode, script } = candidate else {
        return Err("not a shell invocation".into());
    };
    if let Some(path) = script {
        return Ok(ShellInvocation {
            source: ScriptSource::Path(path.clone()),
            dollar0: path.to_string_lossy().into_owned(),
            positional: argv.get(1..).unwrap_or_default().to_vec(),
            set_flags: Vec::new(),
            bash_mode: *bash_mode,
        });
    }

    let arg0 = argv.first().cloned().unwrap_or_else(|| "sh".into());
    let mut index = 1usize;
    let mut set_flags = Vec::new();
    let mut command_string = false;
    while index < argv.len() {
        let arg = &argv[index];
        if arg == "--" {
            index += 1;
            break;
        }
        if arg == "-" {
            return Err("reading a nested shell program from stdin is not implemented".into());
        }
        if arg == "-c" {
            command_string = true;
            index += 1;
            break;
        }
        if arg == "-o" || arg == "+o" {
            let name = argv.get(index + 1).ok_or("shell option requires a name")?;
            set_flags.push(arg.clone());
            set_flags.push(name.clone());
            index += 2;
            continue;
        }
        if let Some(flags) = arg.strip_prefix('-') {
            if flags.is_empty()
                || !flags
                    .chars()
                    .all(|flag| flag == 'c' || SET_FLAGS.contains(flag))
            {
                return Err(format!("unsupported nested shell option {arg:?}"));
            }
            for flag in flags.chars() {
                if flag == 'c' {
                    command_string = true;
                } else {
                    set_flags.push(format!("-{flag}"));
                }
            }
            index += 1;
            if command_string {
                break;
            }
            continue;
        }
        if let Some(flags) = arg.strip_prefix('+') {
            if flags.is_empty() || !flags.chars().all(|flag| SET_FLAGS.contains(flag)) {
                return Err(format!("unsupported nested shell option {arg:?}"));
            }
            set_flags.extend(flags.chars().map(|flag| format!("+{flag}")));
            index += 1;
            continue;
        }
        break;
    }

    if command_string {
        if argv.get(index).map(String::as_str) == Some("--") {
            index += 1;
        }
        let source = argv
            .get(index)
            .ok_or("nested shell -c requires a command")?
            .clone();
        index += 1;
        let dollar0 = argv.get(index).cloned().unwrap_or(arg0);
        let positional = if index < argv.len() {
            argv[index + 1..].to_vec()
        } else {
            Vec::new()
        };
        Ok(ShellInvocation {
            source: ScriptSource::Literal(source),
            dollar0,
            positional,
            set_flags,
            bash_mode: *bash_mode,
        })
    } else {
        let path = argv.get(index).ok_or("nested shell requires a script")?;
        Ok(ShellInvocation {
            source: ScriptSource::Path(PathBuf::from(path)),
            dollar0: path.clone(),
            positional: argv.get(index + 1..).unwrap_or_default().to_vec(),
            set_flags,
            bash_mode: *bash_mode,
        })
    }
}

fn reset_shell(sub: &mut Shell, invocation: &ShellInvocation) {
    sub.traps_mut().clear_all_handlers();
    {
        let options = sub.options_mut();
        options.sh_mode = !invocation.bash_mode;
        options.exit_on_nonzero_command_exit = false;
        options.treat_unset_variables_as_error = false;
        options.print_commands_and_arguments = false;
        options.print_shell_input_lines = false;
        options.disable_filename_globbing = false;
        options.do_not_execute_commands = false;
    }
    let synth = SYNTHETIC_PID.fetch_add(1, Ordering::Relaxed);
    sub.set_snoop_identity(
        invocation.dollar0.clone(),
        invocation.positional.clone(),
        synth,
        invocation.bash_mode,
    );
}

async fn run_shell(
    mut sub: Shell,
    resolved: PathBuf,
    argv: Vec<String>,
    mut params: ExecutionParameters,
    candidate: Candidate,
) -> ExecutionResult {
    let invocation = match parse_shell_invocation(&candidate, &argv) {
        Ok(invocation) => invocation,
        Err(error) => {
            eprintln!("bumba: cannot run nested shell {resolved:?}: {error}");
            return ExecutionResult::new(2);
        }
    };
    let script = match &invocation.source {
        ScriptSource::Literal(script) => script.clone(),
        ScriptSource::Path(path) => {
            let path = if path.is_absolute() {
                path.clone()
            } else {
                sub.absolute_path(path)
            };
            match std::fs::read_to_string(&path) {
                Ok(script) => script,
                Err(error) => {
                    eprintln!("bumba: {path:?}: {error}");
                    return ExecutionResult::new(127);
                }
            }
        }
    };

    params.suppress_errexit = false;
    reset_shell(&mut sub, &invocation);
    if !invocation.set_flags.is_empty() {
        let mut argv = vec!["set".to_owned()];
        argv.extend(invocation.set_flags);
        match sub.run_argv(&argv, &params).await {
            Ok(result) if result.is_success() => {}
            Ok(result) => return result,
            Err(error) => {
                eprintln!("bumba: nested shell options: {error}");
                return ExecutionResult::new(2);
            }
        }
    }

    let source = SourceInfo::from(resolved.clone());
    let result = match sub.run_string(script, &source, &params).await {
        Ok(result) => result,
        Err(error) => {
            eprintln!("bumba: nested shell {resolved:?}: {error}");
            ExecutionResult::new(2)
        }
    };
    let _ = sub.on_exit_with_params(&params).await;
    // An actual child shell communicates only an exit status.  Its `exit`,
    // `return`, loop control, or successful `exec` cannot control the parent
    // interpreter.  `run_string` exposes that internal control flow because we
    // executed in-process, so collapse it at the emulated process boundary.
    ExecutionResult::new(u8::from(result.exit_code))
}

#[derive(Debug)]
struct BumbaInterposer;

impl ExecInterposer<brush_core::extensions::DefaultShellExtensions> for BumbaInterposer {
    fn wants(&self, resolved: &Path) -> bool {
        classify(resolved).is_some()
    }

    fn can_run(&self, resolved: &Path, _argv: &[String]) -> bool {
        classify(resolved).is_some()
    }

    fn run<'a>(
        &'a self,
        mut sub: Shell,
        resolved: PathBuf,
        argv: Vec<String>,
        params: ExecutionParameters,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<ExecutionResult>> + Send + 'a>> {
        Box::pin(async move {
            match classify(&resolved)? {
                Candidate::Builtin(name) => {
                    let mut builtin_argv = Vec::with_capacity(argv.len() + 1);
                    builtin_argv.push("builtin".to_owned());
                    builtin_argv.push(name);
                    builtin_argv.extend(argv.into_iter().skip(1));
                    Some(match sub.run_argv(&builtin_argv, &params).await {
                        Ok(result) => ExecutionResult::new(u8::from(result.exit_code)),
                        Err(error) => {
                            eprintln!("bumba: absolute builtin {resolved:?}: {error}");
                            ExecutionResult::new(126)
                        }
                    })
                }
                candidate @ Candidate::Shell { .. } => {
                    Some(run_shell(sub, resolved, argv, params, candidate).await)
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn interposer_covers_every_optimized_external_registration() {
        let registry = crate::shell::builtins::<
            brush_core::extensions::DefaultShellExtensions,
        >();
        let mut registered = registry
            .iter()
            .filter(|(_, registration)| registration.external_command)
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        let mut covered = super::EXTERNAL_BUILTINS.to_vec();
        registered.sort_unstable();
        covered.sort_unstable();
        assert_eq!(registered, covered);
    }
}
