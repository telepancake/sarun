use std::ffi::OsString;
use std::io::IsTerminal as _;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use brush_core::builtins::{Registration, SimpleCommand, simple_builtin};

#[derive(Clone, Debug)]
pub struct ShellOptions {
    pub sh_mode: bool,
    pub interactive: bool,
    pub shell_name: String,
    pub positional: Vec<String>,
    pub cwd: PathBuf,
}

impl Default for ShellOptions {
    fn default() -> Self {
        Self {
            sh_mode: false,
            interactive: false,
            shell_name: "bumba".into(),
            positional: Vec::new(),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        }
    }
}

struct MakeBuiltin;

impl SimpleCommand for MakeBuiltin {
    fn get_content(
        name: &str,
        _content_type: brush_core::builtins::ContentType,
        _options: &brush_core::builtins::ContentOptions,
    ) -> Result<String, brush_core::error::Error> {
        Ok(format!("{name}: Bumba embedded rkati\n"))
    }

    fn execute<
        SE: brush_core::extensions::ShellExtensions,
        I: Iterator<Item = S>,
        S: AsRef<str>,
    >(
        context: brush_core::commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
        let mut argv: Vec<String> = args.map(|arg| arg.as_ref().to_string()).collect();
        if argv.is_empty() {
            argv.push(context.command_name.clone());
        }
        let cwd = context.shell.working_dir().to_path_buf();
        let shell = &*context.shell;
        let env = context
            .shell
            .env()
            .iter_exported()
            .map(|(name, value)| {
                (
                    OsString::from(name.clone()),
                    OsString::from(value.value().to_cow_str(shell).into_owned()),
                )
            })
            .collect::<Vec<_>>();
        let out = context.try_fd(1).unwrap_or_else(|| std::io::stdout().into());
        let err = context.try_fd(2).unwrap_or_else(|| std::io::stderr().into());
        let recipe_out: Box<dyn Write> = Box::new(
            context.try_fd(1).unwrap_or_else(|| std::io::stdout().into()),
        );
        let recipe_err: Box<dyn Write> = Box::new(
            context.try_fd(2).unwrap_or_else(|| std::io::stderr().into()),
        );
        let stdin = context.try_fd(0);
        let code = crate::make::make_builtin(
            &argv,
            &cwd,
            &env,
            out,
            err,
            recipe_out,
            recipe_err,
            stdin,
            None,
        );
        Ok(brush_core::results::ExecutionResult::new((code & 0xff) as u8))
    }
}

struct NinjaBuiltin;

impl SimpleCommand for NinjaBuiltin {
    fn get_content(
        name: &str,
        _content_type: brush_core::builtins::ContentType,
        _options: &brush_core::builtins::ContentOptions,
    ) -> Result<String, brush_core::error::Error> {
        Ok(format!("{name}: Bumba embedded n2\n"))
    }

    fn execute<
        SE: brush_core::extensions::ShellExtensions,
        I: Iterator<Item = S>,
        S: AsRef<str>,
    >(
        context: brush_core::commands::ExecutionContext<'_, SE>,
        args: I,
    ) -> Result<brush_core::results::ExecutionResult, brush_core::error::Error> {
        let mut argv: Vec<String> = args.map(|arg| arg.as_ref().to_string()).collect();
        if argv.is_empty() {
            argv.push(context.command_name.clone());
        }
        let cwd = context.shell.working_dir().to_path_buf();
        let makeflags = context
            .shell
            .env()
            .get("MAKEFLAGS")
            .map(|(_, value)| OsString::from(value.value().to_cow_str(context.shell).into_owned()));
        let out = context.try_fd(1).unwrap_or_else(|| std::io::stdout().into());
        let err = context.try_fd(2).unwrap_or_else(|| std::io::stderr().into());
        let code = crate::ninja::ninja_builtin_in_environment(
            &argv,
            &cwd,
            makeflags.as_deref(),
            out,
            err,
        );
        Ok(brush_core::results::ExecutionResult::new((code & 0xff) as u8))
    }
}

pub fn builtins<SE: brush_core::extensions::ShellExtensions>() -> std::collections::HashMap<
    String,
    Registration<SE>,
> {
    let mut commands = std::collections::HashMap::new();
    crate::coreutils::extend(&mut commands);
    commands.insert("make".into(), simple_builtin::<MakeBuiltin, SE>());
    commands.insert("gmake".into(), simple_builtin::<MakeBuiltin, SE>());
    commands.insert("ninja".into(), simple_builtin::<NinjaBuiltin, SE>());
    commands.extend(brush_builtins::default_builtins(
        brush_builtins::BuiltinSet::BashMode,
    ));
    crate::find::extend(&mut commands);
    crate::exec_wrappers::extend(&mut commands);
    for name in ["make", "gmake", "ninja"] {
        commands.get_mut(name).expect("inserted above").external_command = true;
    }
    commands
}

fn default_builtins() -> std::collections::HashMap<
    String,
    Registration<brush_core::extensions::DefaultShellExtensions>,
> {
    static REGISTRY: std::sync::OnceLock<
        std::collections::HashMap<
            String,
            Registration<brush_core::extensions::DefaultShellExtensions>,
        >,
    > = std::sync::OnceLock::new();
    REGISTRY.get_or_init(builtins).clone()
}

pub async fn build(options: &ShellOptions) -> Result<brush_core::Shell, brush_core::error::Error> {
    let mut shell = brush_core::Shell::builder()
        .sh_mode(options.sh_mode)
        .interactive(options.interactive)
        .builtins(default_builtins())
        .shell_name(options.shell_name.clone())
        .shell_args(options.positional.clone())
        .working_dir(options.cwd.clone())
        .build()
        .await?;
    if options.interactive && std::env::var_os("PS1").is_none() {
        shell.env_mut().set_global(
            "PS1",
            brush_core::ShellVariable::new(r"bumba\$ "),
        )?;
    }
    // Bumba is an embedding boundary even when used through its standalone
    // CLI.  A root Brush shell would implement `exec` with execve(2), which
    // would replace Bumba before the pathname interposer can handle an
    // Autoconf-style `exec $CONFIG_SHELL ./configure`.  At embedded depth,
    // Brush delegates the command through normal dispatch and then terminates
    // this logical shell with the delegated result: the observable shell
    // behavior is retained without losing the hosting process.
    shell.mark_as_embedded_subshell();
    crate::interpose::install(&mut shell);
    Ok(shell)
}

pub async fn run_script(script: String, options: ShellOptions) -> i32 {
    let mut shell = match build(&options).await {
        Ok(shell) => shell,
        Err(error) => {
            eprintln!("bumba: shell initialization failed: {error}");
            return 127;
        }
    };
    let source = brush_core::SourceInfo::from("-c");
    let params = shell.default_exec_params();
    let code = match shell.run_string(script, &source, &params).await {
        Ok(result) => u8::from(result.exit_code) as i32,
        Err(error) => {
            eprintln!("bumba: {error}");
            2
        }
    };
    if let Err(error) = shell.on_exit_with_params(&params).await {
        eprintln!("bumba: exit trap: {error}");
        return 2;
    }
    code
}

struct NoCompletions;

impl brush_interactive::SemanticCompletionProvider for NoCompletions {
    fn complete(
        &self,
        _source: &str,
        _cursor: usize,
    ) -> Vec<brush_interactive::SemanticCompletion> {
        Vec::new()
    }
}

/// Run the standalone terminal shell without a semantic completion provider.
/// The shared grammar crate can be composed through `run_interactive_with`;
/// Bumba does not grow a second parser while that boundary is being extracted.
pub async fn run_interactive(options: ShellOptions) -> i32 {
    run_interactive_with(options, std::sync::Arc::new(NoCompletions)).await
}

pub async fn run_interactive_with(
    mut options: ShellOptions,
    completion_provider: std::sync::Arc<dyn brush_interactive::SemanticCompletionProvider>,
) -> i32 {
    options.interactive = true;
    let shell = match build(&options).await {
        Ok(shell) => shell,
        Err(error) => {
            eprintln!("bumba: shell initialization failed: {error}");
            return 127;
        }
    };
    let shell_ref: brush_interactive::ShellRef =
        std::sync::Arc::new(tokio::sync::Mutex::new(shell));
    let ui_options = brush_interactive::UIOptions::builder().build();
    let mut input = match brush_interactive::ReedlineInputBackend::new(
        &ui_options,
        &shell_ref,
        completion_provider,
    ) {
        Ok(input) => input,
        Err(error) => {
            eprintln!("bumba: interactive input initialization failed: {error}");
            return 127;
        }
    };
    let interactive_options: brush_interactive::InteractiveOptions = (&ui_options).into();
    let mut interactive = match brush_interactive::InteractiveShell::new(
        &shell_ref,
        &mut input,
        &interactive_options,
    ) {
        Ok(interactive) => interactive,
        Err(error) => {
            eprintln!("bumba: interactive shell initialization failed: {error}");
            return 127;
        }
    };
    if let Err(error) = interactive.run_interactively().await {
        eprintln!("bumba: interactive shell failed: {error}");
        return 1;
    }
    let shell = shell_ref.lock().await;
    i32::from(u8::from(shell.last_exit_status()))
}

thread_local! {
    static RECIPE_CWD: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
    static RECIPE_EDGE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

pub(crate) fn set_recipe_cwd(cwd: Option<PathBuf>) -> Option<PathBuf> {
    RECIPE_CWD.with(|slot| std::mem::replace(&mut *slot.borrow_mut(), cwd))
}

pub(crate) fn set_recipe_edge(edge: Option<String>) -> Option<String> {
    RECIPE_EDGE.with(|slot| std::mem::replace(&mut *slot.borrow_mut(), edge))
}

pub(crate) fn current_recipe_edge() -> Option<String> {
    RECIPE_EDGE.with(|slot| slot.borrow().clone())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecipeStderr {
    Merge,
    Inherit,
    Null,
}

pub type RecipeExecutor = fn(
    prefix: &str,
    command: &str,
    output: &mut dyn FnMut(&[u8]),
    stderr: RecipeStderr,
    stdin: Option<std::os::fd::OwnedFd>,
) -> i32;

static RECIPE_EXECUTOR: std::sync::OnceLock<RecipeExecutor> = std::sync::OnceLock::new();

/// Install the host's recipe executor. This must happen before the first Make
/// recipe in a process. Standalone Bumba defaults to its own Brush executor.
pub fn set_recipe_executor(executor: RecipeExecutor) -> Result<(), RecipeExecutor> {
    RECIPE_EXECUTOR.set(executor)
}

static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();

fn runtime() -> &'static tokio::runtime::Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .thread_stack_size(64 * 1024 * 1024)
            .enable_all()
            .build()
            .expect("Bumba Tokio runtime")
    })
}

pub fn run_recipe(
    prefix: &str,
    command: &str,
    output: &mut dyn FnMut(&[u8]),
    stderr: RecipeStderr,
    stdin: Option<std::os::fd::OwnedFd>,
) -> i32 {
    let executor = *RECIPE_EXECUTOR.get_or_init(|| run_recipe_default);
    executor(prefix, command, output, stderr, stdin)
}

fn run_recipe_default(
    prefix: &str,
    command: &str,
    output: &mut dyn FnMut(&[u8]),
    stderr: RecipeStderr,
    stdin: Option<std::os::fd::OwnedFd>,
) -> i32 {
    let cwd = RECIPE_CWD
        .with(|slot| slot.borrow().clone())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"));
    let (script, exported_env) = match parse_generated_export_prefix(prefix) {
        Some(exported_env) if !prefix.is_empty() => (command.to_string(), Some(exported_env)),
        _ => (format!("{prefix}{command}"), None),
    };
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| {
                handle.block_on(run_recipe_captured(
                    cwd, script, exported_env, output, stderr, stdin,
                ))
            })
        }
        Ok(_) => run_recipe_on_fresh_thread(
            cwd, script, exported_env, output, stderr, stdin,
        ),
        Err(_) => runtime().block_on(run_recipe_captured(
            cwd, script, exported_env, output, stderr, stdin,
        )),
    }
}

/// Decode the exact prefix emitted by kati's export machinery. Handling it as
/// structured initial state avoids reparsing and executing hundreds of
/// `export` statements in every recipe. Any unrecognized prefix returns None
/// and is executed as ordinary shell text, preserving the public runner API.
fn parse_generated_export_prefix(prefix: &str) -> Option<Vec<(String, String)>> {
    let bytes = prefix.as_bytes();
    let mut cursor = 0usize;
    let mut variables = Vec::new();
    while cursor < bytes.len() {
        if bytes[cursor..].starts_with(b"unset ") {
            cursor += 6;
            let end = bytes[cursor..].iter().position(|byte| *byte == b'\n')? + cursor;
            cursor = end + 1;
            continue;
        }
        if !bytes[cursor..].starts_with(b"export ") {
            return None;
        }
        cursor += 7;
        let equals = bytes[cursor..].iter().position(|byte| *byte == b'=')? + cursor;
        let name = std::str::from_utf8(&bytes[cursor..equals]).ok()?.to_string();
        cursor = equals + 1;
        if bytes.get(cursor) != Some(&b'\'') {
            return None;
        }
        cursor += 1;
        let mut value = Vec::new();
        loop {
            if bytes.get(cursor..cursor + 4) == Some(&b"'\\''"[..]) {
                value.push(b'\'');
                cursor += 4;
            } else if bytes.get(cursor) == Some(&b'\'') {
                cursor += 1;
                break;
            } else {
                value.push(*bytes.get(cursor)?);
                cursor += 1;
            }
        }
        if bytes.get(cursor) != Some(&b'\n') {
            return None;
        }
        cursor += 1;
        variables.push((name, String::from_utf8(value).ok()?));
    }
    Some(variables)
}

async fn build_recipe_shell(
    cwd: PathBuf,
    exported_env: Option<Vec<(String, String)>>,
) -> Result<brush_core::Shell, brush_core::error::Error> {
    let replace_environment = exported_env.is_some();
    let mut builder = brush_core::Shell::builder()
        .sh_mode(true)
        .builtins(default_builtins())
        .shell_name("bumba".to_string())
        .working_dir(cwd)
        .do_not_inherit_env(replace_environment);
    if let Some(exported_env) = exported_env {
        for (name, value) in exported_env {
            let mut variable = brush_core::ShellVariable::new(value);
            variable.export();
            builder = builder.var(name, variable);
        }
    }
    let mut shell = builder.build().await?;
    shell.mark_as_embedded_subshell();
    crate::interpose::install(&mut shell);
    Ok(shell)
}

/// Run a recipe on the caller's worker while a reusable Tokio blocking worker
/// drains its output pipe. This keeps output live without creating and joining
/// a second 64 MiB-stack OS thread for every recipe.
async fn run_recipe_captured(
    cwd: PathBuf,
    script: String,
    exported_env: Option<Vec<(String, String)>>,
    output: &mut dyn FnMut(&[u8]),
    stderr: RecipeStderr,
    stdin: Option<std::os::fd::OwnedFd>,
) -> i32 {
    use std::os::fd::AsRawFd as _;

    let (reader, writer) = match std::io::pipe() {
        Ok(pipe) => pipe,
        Err(error) => {
            output(format!("bumba: pipe: {error}\n").as_bytes());
            return 127;
        }
    };
    let flags = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        output(
            format!("bumba: pipe: {}\n", std::io::Error::last_os_error()).as_bytes(),
        );
        return 127;
    }
    let reader = match tokio::io::unix::AsyncFd::new(reader) {
        Ok(reader) => reader,
        Err(error) => {
            output(format!("bumba: pipe: {error}\n").as_bytes());
            return 127;
        }
    };
    let shell_task = async move {
        let mut shell = build_recipe_shell(cwd, exported_env)
            .await
            .map_err(|error| error.to_string())?;
        if let Some(fd) = stdin {
            shell.open_files_mut().set_fd(
                0,
                brush_core::openfiles::OpenFile::from(std::fs::File::from(fd)),
            );
        }
        shell.open_files_mut().set_fd(
            1,
            brush_core::openfiles::OpenFile::from(
                writer.try_clone().map_err(|error| error.to_string())?,
            ),
        );
        match stderr {
            RecipeStderr::Merge => {
                shell.open_files_mut().set_fd(
                    2,
                    brush_core::openfiles::OpenFile::from(writer),
                );
            }
            RecipeStderr::Inherit => {}
            RecipeStderr::Null => {
                let null = std::fs::OpenOptions::new()
                    .write(true)
                    .open("/dev/null")
                    .map_err(|error| error.to_string())?;
                shell.open_files_mut().set_fd(
                    2,
                    brush_core::openfiles::OpenFile::from(null),
                );
            }
        }
        let source = brush_core::SourceInfo::from("<recipe>");
        let params = shell.default_exec_params();
        let result = shell
            .run_string(script, &source, &params)
            .await
            .map_err(|error| error.to_string())?;
        let _ = shell.on_exit_with_params(&params).await;
        Ok::<i32, String>(u8::from(result.exit_code) as i32)
    };
    tokio::pin!(shell_task);
    let mut buffer = [0u8; 8192];
    let result = loop {
        tokio::select! {
            result = &mut shell_task => break result,
            count = read_async_pipe(&reader, &mut buffer) => {
                match count {
                    Ok(0) => break Err("recipe output closed before command completed".to_string()),
                    Ok(count) => output(&buffer[..count]),
                    Err(error) => break Err(format!("pipe: {error}")),
                }
            }
        }
    };
    loop {
        match read_async_pipe(&reader, &mut buffer).await {
            Ok(0) => break,
            Ok(count) => output(&buffer[..count]),
            Err(error) => {
                output(format!("bumba: pipe: {error}\n").as_bytes());
                return 127;
            }
        }
    }
    match result {
        Ok(code) => code,
        Err(error) => {
            output(format!("bumba: {error}\n").as_bytes());
            127
        }
    }
}

async fn read_async_pipe(
    reader: &tokio::io::unix::AsyncFd<std::io::PipeReader>,
    buffer: &mut [u8],
) -> std::io::Result<usize> {
    use std::os::fd::AsRawFd as _;

    loop {
        let mut ready = reader.readable().await?;
        match ready.try_io(|inner| {
            let count = unsafe {
                libc::read(
                    inner.get_ref().as_raw_fd(),
                    buffer.as_mut_ptr().cast::<libc::c_void>(),
                    buffer.len(),
                )
            };
            if count < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(count as usize)
            }
        }) {
            Ok(result) => return result,
            Err(_) => continue,
        }
    }
}

/// A current-thread Tokio runtime cannot be synchronously re-entered. This is
/// a compatibility path for embedders using one; ordinary Kati workers and
/// Bumba's own multi-thread runtime use `run_recipe_captured` directly.
fn run_recipe_on_fresh_thread(
    cwd: PathBuf,
    script: String,
    exported_env: Option<Vec<(String, String)>>,
    output: &mut dyn FnMut(&[u8]),
    stderr: RecipeStderr,
    stdin: Option<std::os::fd::OwnedFd>,
) -> i32 {
    let (mut reader, writer) = match std::io::pipe() {
        Ok(pipe) => pipe,
        Err(error) => {
            output(format!("bumba: pipe: {error}\n").as_bytes());
            return 127;
        }
    };
    let worker = std::thread::Builder::new()
        .name("bumba-recipe-compat".into())
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            runtime().block_on(async move {
                let mut shell = build_recipe_shell(cwd, exported_env)
                    .await
                    .map_err(|error| error.to_string())?;
                if let Some(fd) = stdin {
                    shell.open_files_mut().set_fd(
                        0,
                        brush_core::openfiles::OpenFile::from(std::fs::File::from(fd)),
                    );
                }
                shell.open_files_mut().set_fd(
                    1,
                    brush_core::openfiles::OpenFile::from(
                        writer.try_clone().map_err(|error| error.to_string())?,
                    ),
                );
                match stderr {
                    RecipeStderr::Merge => {
                        shell.open_files_mut().set_fd(
                            2,
                            brush_core::openfiles::OpenFile::from(writer),
                        );
                    }
                    RecipeStderr::Inherit => {}
                    RecipeStderr::Null => {
                        let null = std::fs::OpenOptions::new()
                            .write(true)
                            .open("/dev/null")
                            .map_err(|error| error.to_string())?;
                        shell.open_files_mut().set_fd(
                            2,
                            brush_core::openfiles::OpenFile::from(null),
                        );
                    }
                }
                let source = brush_core::SourceInfo::from("<recipe>");
                let params = shell.default_exec_params();
                let result = shell
                    .run_string(script, &source, &params)
                    .await
                    .map_err(|error| error.to_string())?;
                let _ = shell.on_exit_with_params(&params).await;
                Ok::<i32, String>(u8::from(result.exit_code) as i32)
            })
        });
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => output(&buffer[..count]),
        }
    }
    match worker {
        Ok(worker) => match worker.join() {
            Ok(Ok(code)) => code,
            Ok(Err(error)) => {
                output(format!("bumba: {error}\n").as_bytes());
                127
            }
            Err(_) => 127,
        },
        Err(error) => {
            output(format!("bumba: recipe worker: {error}\n").as_bytes());
            127
        }
    }
}

pub(crate) fn ninja_executor(
    command: &str,
    output: &mut dyn FnMut(&[u8]),
) -> n2::process::Termination {
    let previous = set_recipe_cwd(n2::graph::get_cwd());
    let code = run_recipe("", command, output, RecipeStderr::Merge, None);
    set_recipe_cwd(previous);
    if code == 0 {
        n2::process::Termination::Success
    } else {
        n2::process::Termination::Failure
    }
}

const HELP: &str = "Bumba — single-process shell and build executor

Usage:
  bumba [OPTIONS] [SCRIPT [ARG ...]]
  bumba -c COMMAND [NAME [ARG ...]]
  bumba make [MAKE-ARG ...]
  bumba ninja [NINJA-ARG ...]

With no SCRIPT, Bumba opens an interactive shell when standard input is a
terminal and reads a script from standard input otherwise.

Options:
  -c COMMAND       Execute COMMAND
  -i, --interactive
                   Use interactive shell semantics; with no command or script,
                   open the terminal prompt
  -s, --stdin      Read commands from standard input
  -h, --help       Print this help
  -V, --version    Print the version
";

enum CliAction {
    Help,
    Version,
    Interactive(ShellOptions),
    Script(String, ShellOptions),
}

struct CliError {
    message: String,
    code: i32,
}

impl CliError {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 2,
        }
    }

    fn input(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 1,
        }
    }

    fn script(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: 127,
        }
    }
}

fn parse_cli(argv: &[String], role: &str) -> Result<CliAction, CliError> {
    let mut options = ShellOptions {
        sh_mode: matches!(role, "sh" | "dash"),
        shell_name: role.to_owned(),
        ..ShellOptions::default()
    };
    let mut index = 1usize;
    let mut force_interactive = false;
    let mut force_stdin = false;
    let mut command = None;

    while let Some(arg) = argv.get(index) {
        match arg.as_str() {
            "-h" | "--help" => return Ok(CliAction::Help),
            "-V" | "--version" => return Ok(CliAction::Version),
            "-i" | "--interactive" => {
                force_interactive = true;
                index += 1;
            }
            "-s" | "--stdin" => {
                force_stdin = true;
                index += 1;
                break;
            }
            "-c" => {
                index += 1;
                command = Some(
                    argv.get(index)
                        .ok_or_else(|| CliError::usage("option -c requires a command"))?
                        .clone(),
                );
                index += 1;
                break;
            }
            "--" => {
                index += 1;
                break;
            }
            "-" => {
                force_stdin = true;
                index += 1;
                break;
            }
            _ if arg.starts_with('-') => {
                return Err(CliError::usage(format!("unknown option {arg:?}")));
            }
            _ => break,
        }
    }

    if let Some(command) = command {
        if let Some(name) = argv.get(index) {
            options.shell_name = name.clone();
            index += 1;
        }
        options.positional = argv.get(index..).unwrap_or_default().to_vec();
        options.interactive = force_interactive;
        return Ok(CliAction::Script(command, options));
    }

    if force_stdin {
        options.positional = argv.get(index..).unwrap_or_default().to_vec();
        options.interactive = force_interactive;
        let mut script = String::new();
        std::io::stdin()
            .read_to_string(&mut script)
            .map_err(|error| CliError::input(format!("standard input: {error}")))?;
        return Ok(CliAction::Script(script, options));
    }

    if let Some(path) = argv.get(index) {
        options.shell_name = path.clone();
        options.positional = argv.get(index + 1..).unwrap_or_default().to_vec();
        options.interactive = force_interactive;
        let script = std::fs::read_to_string(path)
            .map_err(|error| CliError::script(format!("{path}: {error}")))?;
        return Ok(CliAction::Script(script, options));
    }

    if force_interactive || std::io::stdin().is_terminal() {
        return Ok(CliAction::Interactive(options));
    }

    let mut script = String::new();
    std::io::stdin()
        .read_to_string(&mut script)
        .map_err(|error| CliError::input(format!("standard input: {error}")))?;
    Ok(CliAction::Script(script, options))
}

pub fn run(argv: &[String]) -> i32 {
    let role = Path::new(argv.first().map_or("bumba", String::as_str))
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("bumba");
    if matches!(role, "make" | "gmake") {
        return crate::make::make_main(argv);
    }
    if role == "ninja" {
        return crate::ninja::n2_main(argv);
    }
    if argv.get(1).is_some_and(|arg| matches!(arg.as_str(), "make" | "gmake")) {
        let mut forwarded = argv[1..].to_vec();
        forwarded[0] = "make".into();
        return crate::make::make_main(&forwarded);
    }
    if argv.get(1).is_some_and(|arg| arg == "ninja") {
        let mut forwarded = argv[1..].to_vec();
        forwarded[0] = "ninja".into();
        return crate::ninja::n2_main(&forwarded);
    }

    match parse_cli(argv, role) {
        Ok(CliAction::Help) => {
            print!("{HELP}");
            0
        }
        Ok(CliAction::Version) => {
            println!("bumba {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Ok(CliAction::Interactive(options)) => runtime().block_on(run_interactive(options)),
        Ok(CliAction::Script(script, options)) => runtime().block_on(run_script(script, options)),
        Err(error) => {
            eprintln!("bumba: {}", error.message);
            if error.code == 2 {
                eprintln!("Try 'bumba --help' for more information.");
            }
            error.code
        }
    }
}
