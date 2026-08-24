use std::ffi::OsString;
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
        let out = context.try_fd(1).unwrap_or_else(|| std::io::stdout().into());
        let err = context.try_fd(2).unwrap_or_else(|| std::io::stderr().into());
        let code = crate::ninja::ninja_builtin(&argv, &cwd, out, err);
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

pub async fn build(options: &ShellOptions) -> Result<brush_core::Shell, brush_core::error::Error> {
    let mut shell = brush_core::Shell::builder()
        .sh_mode(options.sh_mode)
        .interactive(options.interactive)
        .builtins(builtins())
        .shell_name(options.shell_name.clone())
        .shell_args(options.positional.clone())
        .working_dir(options.cwd.clone())
        .build()
        .await?;
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
    match shell.run_string(script, &source, &params).await {
        Ok(result) => u8::from(result.exit_code) as i32,
        Err(error) => {
            eprintln!("bumba: {error}");
            2
        }
    }
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
    let script = format!("{prefix}{command}");
    let (mut reader, writer) = match std::io::pipe() {
        Ok(pipe) => pipe,
        Err(error) => {
            output(format!("bumba: pipe: {error}\n").as_bytes());
            return 127;
        }
    };
    let worker = std::thread::Builder::new()
        .name("bumba-recipe".into())
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            runtime().block_on(async move {
                let options = ShellOptions { sh_mode: true, cwd, ..ShellOptions::default() };
                let mut shell = build(&options).await.map_err(|error| error.to_string())?;
                shell.mark_as_embedded_subshell();
                if let Some(fd) = stdin {
                    shell.open_files_mut().set_fd(
                        0,
                        brush_core::openfiles::OpenFile::from(std::fs::File::from(fd)),
                    );
                }
                shell.open_files_mut().set_fd(
                    1,
                    brush_core::openfiles::OpenFile::from(writer.try_clone().map_err(|e| e.to_string())?),
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

    let mut options = ShellOptions::default();
    let script = if argv.get(1).is_some_and(|arg| arg == "-c") {
        match argv.get(2) {
            Some(script) => {
                options.positional = argv.get(3..).unwrap_or_default().to_vec();
                script.clone()
            }
            None => {
                eprintln!("bumba: -c requires a script");
                return 2;
            }
        }
    } else if let Some(path) = argv.get(1) {
        options.shell_name = path.clone();
        options.positional = argv.get(2..).unwrap_or_default().to_vec();
        match std::fs::read_to_string(path) {
            Ok(script) => script,
            Err(error) => {
                eprintln!("bumba: {path}: {error}");
                return 127;
            }
        }
    } else {
        let mut script = String::new();
        if let Err(error) = std::io::stdin().read_to_string(&mut script) {
            eprintln!("bumba: stdin: {error}");
            return 1;
        }
        script
    };

    runtime().block_on(run_script(script, options))
}
