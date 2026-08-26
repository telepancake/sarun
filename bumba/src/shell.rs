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

/// Brush-owned logical descriptors carried opaquely through Kati to recipe
/// workers. Keeping the concrete type here lets Kati remain independent of
/// Brush while recipes preserve redirects and pipeline endpoints exactly.
#[derive(Clone)]
pub(crate) struct LogicalRecipeIo {
    pub(crate) stdout: brush_core::openfiles::OpenFile,
    pub(crate) stderr: brush_core::openfiles::OpenFile,
}

struct MakeInvocation {
    argv: Vec<String>,
    cwd: PathBuf,
    env: Vec<(OsString, OsString)>,
    out: brush_core::openfiles::OpenFile,
    err: brush_core::openfiles::OpenFile,
    recipe_out: brush_core::openfiles::OpenFile,
    recipe_err: brush_core::openfiles::OpenFile,
    stdin: Option<brush_core::openfiles::OpenFile>,
}

impl MakeInvocation {
    fn capture<SE: brush_core::extensions::ShellExtensions>(
        context: brush_core::commands::ExecutionContext<'_, SE>,
        mut argv: Vec<String>,
    ) -> Self {
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
        let recipe_out = context.try_fd(1).unwrap_or_else(|| std::io::stdout().into());
        let recipe_err = context.try_fd(2).unwrap_or_else(|| std::io::stderr().into());
        let stdin = context.try_fd(0);
        Self { argv, cwd, env, out, err, recipe_out, recipe_err, stdin }
    }

    fn output_needs_concurrent_reader(&self) -> bool {
        [&self.recipe_out, &self.recipe_err].into_iter().any(|output| {
            matches!(
                output,
                brush_core::openfiles::OpenFile::PipeWriter(_)
                    | brush_core::openfiles::OpenFile::MemoryPipeWriter(_)
                    | brush_core::openfiles::OpenFile::Stream(_)
            )
        })
    }

    fn run(self) -> i32 {
        crate::make::make_builtin(
            &self.argv,
            &self.cwd,
            &self.env,
            self.out,
            self.err,
            self.recipe_out,
            self.recipe_err,
            self.stdin,
            None,
        )
    }
}

fn execute_make_builtin<SE: brush_core::extensions::ShellExtensions>(
    context: brush_core::commands::ExecutionContext<'_, SE>,
    args: Vec<brush_core::commands::CommandArg>,
) -> brush_core::builtins::BoxFuture<
    '_,
    Result<brush_core::results::ExecutionResult, brush_core::error::Error>,
> {
    Box::pin(async move {
        let argv = args.into_iter().map(|arg| arg.to_string()).collect();
        let invocation = MakeInvocation::capture(context, argv);
        let code = if invocation.output_needs_concurrent_reader() {
            tokio::task::spawn_blocking(move || invocation.run())
                .await
                .map_err(|error| {
                    brush_core::error::Error::from(std::io::Error::other(format!(
                        "embedded make worker failed: {error}"
                    )))
                })?
        } else {
            invocation.run()
        };
        Ok(brush_core::results::ExecutionResult::new((code & 0xff) as u8))
    })
}

fn make_registration<SE: brush_core::extensions::ShellExtensions>() -> Registration<SE> {
    let mut registration = simple_builtin::<MakeBuiltin, SE>();
    registration.execute_func = execute_make_builtin::<SE>;
    registration
}

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
        let invocation = MakeInvocation::capture(
            context,
            args.map(|arg| arg.as_ref().to_string()).collect(),
        );
        let code = invocation.run();
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
    commands.insert("make".into(), make_registration::<SE>());
    commands.insert("gmake".into(), make_registration::<SE>());
    commands.insert("ninja".into(), simple_builtin::<NinjaBuiltin, SE>());
    commands.extend(brush_builtins::default_builtins(
        brush_builtins::BuiltinSet::BashMode,
    ));
    // Only leaf shell builtins opt into descriptor-free pipeline transport.
    // Dispatcher builtins (`command`, `eval`, `exec`, `.`, `source`,
    // `builtin`) can run arbitrary external commands and must retain kernel
    // descriptors even though their outer command is itself a builtin.
    for name in [
        ":", "false", "true", "echo", "printf", "test", "[", "pwd", "help",
        "type", "alias", "dirs", "caller", "times", "umask", "set", "shopt", "let",
    ] {
        if let Some(registration) = commands.get_mut(name) {
            registration.userspace_pipe_safe = true;
        }
    }
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
static LITERAL_EXTERNAL_LAUNCHES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(crate) fn literal_external_launches() -> u64 {
    LITERAL_EXTERNAL_LAUNCHES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Install the host's recipe executor. This must happen before the first Make
/// recipe in a process. Standalone Bumba defaults to its own Brush executor.
pub fn set_recipe_executor(executor: RecipeExecutor) -> Result<(), RecipeExecutor> {
    RECIPE_EXECUTOR.set(executor)
}

static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();

thread_local! {
    /// Recipe pools already provide OS-level parallelism. Give each persistent
    /// worker its own reactor instead of making all recipe waits contend on a
    /// second process-wide Tokio scheduler.
    static RECIPE_RUNTIME: std::cell::RefCell<Option<tokio::runtime::Runtime>> =
        const { std::cell::RefCell::new(None) };
}

fn recipe_block_on<F: std::future::Future>(future: F) -> F::Output {
    RECIPE_RUNTIME.with(|slot| {
        if slot.borrow().is_none() {
            *slot.borrow_mut() = Some(
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Bumba recipe runtime"),
            );
        }
        slot.borrow().as_ref().unwrap().block_on(future)
    })
}

fn runtime() -> &'static tokio::runtime::Runtime {
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            // Kati/N2 recipe workers drive their own top-level futures. Tokio
            // workers are auxiliary capacity for spawned pipeline/process
            // tasks; mirroring every CPU here only adds scheduler contention.
            .worker_threads(2)
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
    if let Some(code) = run_literal_external_captured(
        &cwd,
        &script,
        exported_env.as_deref(),
        output,
        stderr,
        stdin.as_ref(),
    ) {
        return code;
    }
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
        Err(_) => recipe_block_on(run_recipe_captured(
            cwd, script, exported_env, output, stderr, stdin,
        )),
    }
}

/// Standalone make output already targets the process descriptors. Avoid the
/// per-recipe capture pipe and async readback; embedded makes continue using
/// `run_recipe` so their caller's logical descriptors and event stream remain
/// authoritative.
pub(crate) fn run_recipe_direct(
    prefix: &str,
    command: &str,
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
    if let Some(code) = run_literal_external_direct(
        &cwd,
        &script,
        exported_env.as_deref(),
        stderr,
        stdin.as_ref(),
    ) {
        return code;
    }
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| {
                handle.block_on(run_recipe_uncaptured(
                    cwd, script, exported_env, stderr, stdin,
                ))
            })
        }
        Ok(_) => std::thread::Builder::new()
            .name("bumba-recipe-direct-compat".into())
            .stack_size(64 * 1024 * 1024)
            .spawn(move || {
                runtime().block_on(run_recipe_uncaptured(
                    cwd, script, exported_env, stderr, stdin,
                ))
            })
            .and_then(|thread| {
                thread.join().map_err(|_| std::io::Error::other("recipe worker panicked"))
            })
            .unwrap_or(127),
        Err(_) => recipe_block_on(run_recipe_uncaptured(
            cwd, script, exported_env, stderr, stdin,
        )),
    }
}

/// Run an embedded make recipe directly against the invoking Brush shell's
/// logical descriptors. Unlike `run_recipe`, this introduces no intermediate
/// capture pipe, so a synchronous recursive make cannot fill a pipe whose
/// reader is suspended above it in the same shell future.
pub(crate) fn run_recipe_inherited(
    prefix: &str,
    command: &str,
    io: &LogicalRecipeIo,
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
    if let Some(code) = run_literal_external_inherited(
        &cwd,
        &script,
        exported_env.as_deref(),
        io,
        stderr,
        stdin.as_ref(),
    ) {
        return code;
    }
    let stdout = io.stdout.clone();
    let logical_stderr = io.stderr.clone();
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| {
                handle.block_on(run_recipe_with_io(
                    cwd, script, exported_env, stdout, logical_stderr, stderr, stdin,
                ))
            })
        }
        Ok(_) => std::thread::Builder::new()
            .name("bumba-recipe-inherited-compat".into())
            .stack_size(64 * 1024 * 1024)
            .spawn(move || {
                runtime().block_on(run_recipe_with_io(
                    cwd, script, exported_env, stdout, logical_stderr, stderr, stdin,
                ))
            })
            .and_then(|thread| {
                thread.join().map_err(|_| std::io::Error::other("recipe worker panicked"))
            })
            .unwrap_or(127),
        Err(_) => recipe_block_on(run_recipe_with_io(
            cwd, script, exported_env, stdout, logical_stderr, stderr, stdin,
        )),
    }
}

async fn run_recipe_uncaptured(
    cwd: PathBuf,
    script: String,
    exported_env: Option<Vec<(String, String)>>,
    stderr: RecipeStderr,
    stdin: Option<std::os::fd::OwnedFd>,
) -> i32 {
    let mut shell = match build_recipe_shell(cwd, exported_env).await {
        Ok(shell) => shell,
        Err(error) => {
            eprintln!("bumba: {error}");
            return 127;
        }
    };
    if let Some(fd) = stdin {
        shell.open_files_mut().set_fd(
            0,
            brush_core::openfiles::OpenFile::from(std::fs::File::from(fd)),
        );
    }
    shell.open_files_mut().set_fd(1, std::io::stdout().into());
    match stderr {
        RecipeStderr::Merge => {
            shell.open_files_mut().set_fd(2, std::io::stdout().into());
        }
        RecipeStderr::Inherit => {
            shell.open_files_mut().set_fd(2, std::io::stderr().into());
        }
        RecipeStderr::Null => {
            let null = match std::fs::OpenOptions::new().write(true).open("/dev/null") {
                Ok(null) => null,
                Err(error) => {
                    eprintln!("bumba: /dev/null: {error}");
                    return 127;
                }
            };
            shell.open_files_mut().set_fd(
                2,
                brush_core::openfiles::OpenFile::from(null),
            );
        }
    }
    match execute_recipe_script(&mut shell, script).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("bumba: {error}");
            127
        }
    }
}

async fn run_recipe_with_io(
    cwd: PathBuf,
    script: String,
    exported_env: Option<Vec<(String, String)>>,
    stdout: brush_core::openfiles::OpenFile,
    logical_stderr: brush_core::openfiles::OpenFile,
    stderr: RecipeStderr,
    stdin: Option<std::os::fd::OwnedFd>,
) -> i32 {
    let mut diagnostic = logical_stderr.clone();
    let mut shell = match build_recipe_shell(cwd, exported_env).await {
        Ok(shell) => shell,
        Err(error) => {
            let _ = writeln!(diagnostic, "bumba: {error}");
            return 127;
        }
    };
    if let Some(fd) = stdin {
        shell.open_files_mut().set_fd(
            0,
            brush_core::openfiles::OpenFile::from(std::fs::File::from(fd)),
        );
    }
    shell.open_files_mut().set_fd(1, stdout.clone());
    match stderr {
        RecipeStderr::Merge => {
            shell.open_files_mut().set_fd(2, stdout);
        }
        RecipeStderr::Inherit => {
            shell.open_files_mut().set_fd(2, logical_stderr);
        }
        RecipeStderr::Null => {
            let null = match std::fs::OpenOptions::new().write(true).open("/dev/null") {
                Ok(null) => null,
                Err(error) => {
                    let _ = writeln!(diagnostic, "bumba: /dev/null: {error}");
                    return 127;
                }
            };
            shell.open_files_mut().set_fd(
                2,
                brush_core::openfiles::OpenFile::from(null),
            );
        }
    }
    match execute_recipe_script(&mut shell, script).await {
        Ok(code) => code,
        Err(error) => {
            let _ = writeln!(diagnostic, "bumba: {error}");
            127
        }
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

thread_local! {
    /// Building a Brush shell initializes immutable registries and default
    /// state. Recipe workers reuse one pristine template for inherited-env
    /// recipes and one for fully structured environments, then clone the
    /// appropriate template for each logical subshell.
    static RECIPE_SHELL_TEMPLATES: std::cell::RefCell<(
        Option<brush_core::Shell>,
        Option<brush_core::Shell>,
    )> = const { std::cell::RefCell::new((None, None)) };
}

async fn recipe_shell_template(
    replace_environment: bool,
) -> Result<brush_core::Shell, brush_core::error::Error> {
    if let Some(shell) = RECIPE_SHELL_TEMPLATES.with(|templates| {
        let templates = templates.borrow();
        if replace_environment { &templates.1 } else { &templates.0 }.clone()
    }) {
        return Ok(shell);
    }
    let mut shell = brush_core::Shell::builder()
        .sh_mode(true)
        .builtins(default_builtins())
        .shell_name("bumba".to_string())
        .working_dir(PathBuf::from("/"))
        .do_not_inherit_env(replace_environment)
        .build()
        .await?;
    crate::interpose::install(&mut shell);
    RECIPE_SHELL_TEMPLATES.with(|templates| {
        let mut templates = templates.borrow_mut();
        if replace_environment {
            templates.1 = Some(shell.clone());
        } else {
            templates.0 = Some(shell.clone());
        }
    });
    Ok(shell)
}

async fn build_recipe_shell(
    cwd: PathBuf,
    exported_env: Option<Vec<(String, String)>>,
) -> Result<brush_core::Shell, brush_core::error::Error> {
    let replace_environment = exported_env.is_some();
    let mut shell = recipe_shell_template(replace_environment).await?;
    shell.set_working_dir(&cwd)?;
    if let Some(exported_env) = exported_env {
        for (name, value) in exported_env {
            let mut variable = brush_core::ShellVariable::new(value);
            variable.export();
            shell.env_mut().set_global(name, variable)?;
        }
    }
    shell.mark_as_embedded_subshell();
    Ok(shell)
}

/// Split a deliberately small shell-language subset into already-expanded
/// words. Quotes and backslash may only remove quoting; expansion, globbing,
/// assignments, redirects, separators, and control syntax remain ineligible.
fn literal_recipe_argv(script: &str) -> Option<Vec<String>> {
    let bytes = script.as_bytes();
    let mut argv = Vec::new();
    let mut word = Vec::new();
    let mut word_started = false;
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if matches!(byte, b' ' | b'\t') {
            if word_started {
                argv.push(String::from_utf8(std::mem::take(&mut word)).ok()?);
                word_started = false;
            }
            cursor += 1;
            continue;
        }
        if byte == b'\'' {
            word_started = true;
            cursor += 1;
            let end = bytes[cursor..].iter().position(|next| *next == b'\'')? + cursor;
            if bytes[cursor..end].contains(&b'\n') {
                return None;
            }
            word.extend_from_slice(&bytes[cursor..end]);
            cursor = end + 1;
            continue;
        }
        if byte == b'"' {
            word_started = true;
            cursor += 1;
            loop {
                let byte = *bytes.get(cursor)?;
                if byte == b'"' {
                    cursor += 1;
                    break;
                }
                if matches!(byte, b'$' | b'`' | b'\n') {
                    return None;
                }
                if byte == b'\\' {
                    let next = *bytes.get(cursor + 1)?;
                    if matches!(next, b'$' | b'`' | b'"' | b'\\') {
                        word.push(next);
                    } else {
                        word.extend_from_slice(&[byte, next]);
                    }
                    cursor += 2;
                } else {
                    word.push(byte);
                    cursor += 1;
                }
            }
            continue;
        }
        if byte == b'\\' {
            let next = *bytes.get(cursor + 1)?;
            if next == b'\n' {
                return None;
            }
            word_started = true;
            word.push(next);
            cursor += 2;
            continue;
        }
        if !byte.is_ascii_graphic()
            || matches!(
                byte,
                b'|' | b'&' | b';' | b'<' | b'>' | b'(' | b')' | b'[' | b']'
                    | b'{' | b'}' | b'*' | b'?' | b'~' | b'#' | b'$' | b'`' | b'!'
            )
        {
            return None;
        }
        word_started = true;
        word.push(byte);
        cursor += 1;
    }
    if word_started {
        argv.push(String::from_utf8(word).ok()?);
    }
    let command = argv.first()?;
    if command.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.bytes().enumerate().all(|(index, byte)| {
                byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit())
            })
    }) {
        return None;
    }
    Some(argv)
}

fn shell_builtin_names() -> &'static std::collections::HashSet<String> {
    static NAMES: std::sync::LazyLock<std::collections::HashSet<String>> =
        std::sync::LazyLock::new(|| default_builtins().into_keys().collect());
    &NAMES
}

/// Return an already-resolved external command only when the recipe contains
/// no shell language and neither Brush's builtin table nor Bumba's pathname
/// interposer owns it. Resolution happens before the decision so a PATH entry,
/// absolute SDK path, or shell-script shebang cannot evade interposition.
fn literal_external_command(
    cwd: &Path,
    script: &str,
    exported_env: Option<&[(String, String)]>,
) -> Option<(PathBuf, Vec<String>)> {
    use std::os::unix::fs::PermissionsExt as _;

    let argv = literal_recipe_argv(script)?;
    let command = argv.first()?;
    if !command.contains('/') && shell_builtin_names().contains(command) {
        return None;
    }
    let executable = if command.contains('/') {
        let path = PathBuf::from(command);
        if path.is_absolute() { path } else { cwd.join(path) }
    } else {
        let path = exported_env
            .and_then(|env| env.iter().rev().find(|(name, _)| name == "PATH"))
            .map(|(_, value)| std::ffi::OsString::from(value))
            .or_else(|| std::env::var_os("PATH"))?;
        std::env::split_paths(&path).find_map(|directory| {
            let directory = if directory.as_os_str().is_empty() {
                cwd.to_owned()
            } else if directory.is_absolute() {
                directory
            } else {
                cwd.join(directory)
            };
            let candidate = directory.join(command);
            let metadata = std::fs::metadata(&candidate).ok()?;
            (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
                .then_some(candidate)
        })?
    };
    let metadata = std::fs::metadata(&executable).ok()?;
    if !metadata.is_file()
        || metadata.permissions().mode() & 0o111 == 0
        || crate::interpose::owns_resolved_path(&executable)
    {
        return None;
    }
    Some((executable, argv))
}

fn external_process(
    executable: &Path,
    argv: &[String],
    cwd: &Path,
    exported_env: Option<&[(String, String)]>,
    stdin: Option<&std::os::fd::OwnedFd>,
) -> std::io::Result<std::process::Command> {
    use std::os::unix::process::CommandExt as _;

    let mut process = std::process::Command::new(executable);
    process.arg0(&argv[0]).args(&argv[1..]).current_dir(cwd);
    if let Some(exported_env) = exported_env {
        process
            .env_clear()
            .envs(exported_env.iter().map(|(name, value)| (name, value)));
    }
    if let Some(stdin) = stdin {
        process.stdin(std::process::Stdio::from(std::fs::File::from(stdin.try_clone()?)));
    }
    Ok(process)
}

fn external_exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt as _;

    status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

fn run_literal_external_direct(
    cwd: &Path,
    script: &str,
    exported_env: Option<&[(String, String)]>,
    stderr: RecipeStderr,
    stdin: Option<&std::os::fd::OwnedFd>,
) -> Option<i32> {
    let (executable, argv) = literal_external_command(cwd, script, exported_env)?;
    LITERAL_EXTERNAL_LAUNCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut process = match external_process(&executable, &argv, cwd, exported_env, stdin) {
        Ok(process) => process,
        Err(error) => {
            eprintln!("bumba: {executable:?}: {error}");
            return Some(127);
        }
    };
    match stderr {
        RecipeStderr::Merge => {
            process.stderr(std::process::Stdio::from(std::io::stdout()));
        }
        RecipeStderr::Inherit => {}
        RecipeStderr::Null => {
            process.stderr(std::process::Stdio::null());
        }
    }
    Some(match process.status() {
        Ok(status) => external_exit_code(status),
        Err(error) => {
            eprintln!("bumba: {executable:?}: {error}");
            127
        }
    })
}

fn run_literal_external_inherited(
    cwd: &Path,
    script: &str,
    exported_env: Option<&[(String, String)]>,
    io: &LogicalRecipeIo,
    stderr: RecipeStderr,
    stdin: Option<&std::os::fd::OwnedFd>,
) -> Option<i32> {
    let (executable, argv) = literal_external_command(cwd, script, exported_env)?;
    LITERAL_EXTERNAL_LAUNCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut diagnostic = io.stderr.clone();
    let mut process = match external_process(&executable, &argv, cwd, exported_env, stdin) {
        Ok(process) => process,
        Err(error) => {
            let _ = writeln!(diagnostic, "bumba: {executable:?}: {error}");
            return Some(127);
        }
    };
    let stdout = match std::process::Stdio::try_from(io.stdout.clone()) {
        Ok(stdout) => stdout,
        Err(error) => {
            let _ = writeln!(diagnostic, "bumba: stdout: {error}");
            return Some(127);
        }
    };
    process.stdout(stdout);
    match stderr {
        RecipeStderr::Merge => match std::process::Stdio::try_from(io.stdout.clone()) {
            Ok(stderr) => {
                process.stderr(stderr);
            }
            Err(error) => {
                let _ = writeln!(diagnostic, "bumba: stderr: {error}");
                return Some(127);
            }
        },
        RecipeStderr::Inherit => match std::process::Stdio::try_from(io.stderr.clone()) {
            Ok(stderr) => {
                process.stderr(stderr);
            }
            Err(error) => {
                let _ = writeln!(diagnostic, "bumba: stderr: {error}");
                return Some(127);
            }
        },
        RecipeStderr::Null => {
            process.stderr(std::process::Stdio::null());
        }
    }
    Some(match process.status() {
        Ok(status) => external_exit_code(status),
        Err(error) => {
            let _ = writeln!(diagnostic, "bumba: {executable:?}: {error}");
            127
        }
    })
}

fn run_literal_external_captured(
    cwd: &Path,
    script: &str,
    exported_env: Option<&[(String, String)]>,
    output: &mut dyn FnMut(&[u8]),
    stderr: RecipeStderr,
    stdin: Option<&std::os::fd::OwnedFd>,
) -> Option<i32> {
    use std::io::Read as _;

    let (executable, argv) = literal_external_command(cwd, script, exported_env)?;
    LITERAL_EXTERNAL_LAUNCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (mut reader, writer) = match std::io::pipe() {
        Ok(pipe) => pipe,
        Err(error) => {
            output(format!("bumba: pipe: {error}\n").as_bytes());
            return Some(127);
        }
    };
    let mut process = match external_process(&executable, &argv, cwd, exported_env, stdin) {
        Ok(process) => process,
        Err(error) => {
            output(format!("bumba: {executable:?}: {error}\n").as_bytes());
            return Some(127);
        }
    };
    let stdout = match writer.try_clone() {
        Ok(stdout) => stdout,
        Err(error) => {
            output(format!("bumba: pipe: {error}\n").as_bytes());
            return Some(127);
        }
    };
    let stdout: std::os::fd::OwnedFd = stdout.into();
    process.stdout(std::process::Stdio::from(stdout));
    match stderr {
        RecipeStderr::Merge => {
            let writer: std::os::fd::OwnedFd = writer.into();
            process.stderr(std::process::Stdio::from(writer));
        }
        RecipeStderr::Inherit => drop(writer),
        RecipeStderr::Null => {
            process.stderr(std::process::Stdio::null());
            drop(writer);
        }
    }
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) => {
            output(format!("bumba: {executable:?}: {error}\n").as_bytes());
            return Some(127);
        }
    };
    // `Command` retains its configured descriptors so it can be spawned more
    // than once. Close those parent-side pipe writers before waiting for EOF.
    drop(process);
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => output(&buffer[..count]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                output(format!("bumba: pipe: {error}\n").as_bytes());
                let _ = child.wait();
                return Some(127);
            }
        }
    }
    Some(match child.wait() {
        Ok(status) => external_exit_code(status),
        Err(error) => {
            output(format!("bumba: {executable:?}: {error}\n").as_bytes());
            127
        }
    })
}

async fn execute_recipe_script(
    shell: &mut brush_core::Shell,
    script: String,
) -> Result<i32, String> {
    let params = shell.default_exec_params();
    let result = if let Some(argv) = literal_recipe_argv(&script) {
        shell.run_argv(&argv, &params).await
    } else {
        let source = brush_core::SourceInfo::from("<recipe>");
        shell.run_string(script, &source, &params).await
    }
    .map_err(|error| error.to_string())?;
    let _ = shell.on_exit_with_params(&params).await;
    Ok(u8::from(result.exit_code) as i32)
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
        execute_recipe_script(&mut shell, script).await
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
                execute_recipe_script(&mut shell, script).await
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

#[cfg(test)]
mod recipe_fast_path_tests {
    use super::{literal_external_command, literal_recipe_argv};

    #[test]
    fn literal_argv_accepts_only_preexpanded_words() {
        assert_eq!(
            literal_recipe_argv("cc -Iinclude -DVALUE=3 src/main.c"),
            Some(vec![
                "cc".into(), "-Iinclude".into(), "-DVALUE=3".into(), "src/main.c".into()
            ])
        );
        assert_eq!(literal_recipe_argv(":"), Some(vec![":".into()]));
        assert_eq!(
            literal_recipe_argv("cc -DARCH='\"armv8.5-a\"' one\\ two.c"),
            Some(vec!["cc".into(), "-DARCH=\"armv8.5-a\"".into(), "one two.c".into()]),
        );
    }

    #[test]
    fn literal_argv_rejects_every_shell_semantic() {
        for script in [
            "CC=clang cc main.c",
            "echo $HOME",
            "cat < input",
            "a && b",
            "echo *.c",
            "echo hi # comment",
            "echo first\necho second",
            "echo \"$HOME\"",
            "echo 'unterminated",
        ] {
            assert!(literal_recipe_argv(script).is_none(), "accepted {script:?}");
        }
    }

    #[test]
    fn literal_external_path_never_bypasses_owned_commands() {
        let cwd = std::env::current_dir().unwrap();
        assert!(literal_external_command(&cwd, "/bin/cat /dev/null", None).is_none());
        assert!(literal_external_command(&cwd, "/bin/ls /", None).is_some());
    }
}
