// Argument parsing + dispatch for the `oaita` duty. Both entry shapes route
// here:
//   sarun oaita gen NAME …      (via main.rs subcommand dispatch)
//   oaita gen NAME …            (via main.rs argv[0]-basename dispatch)
//
// Argparse is hand-rolled to keep the dep tree small (clap would pull a
// crate-set of comparable size to all of oaita itself). The grammar is
// small enough that one function per subcommand is honest.

use std::io::Read;

use crate::oaita::config::Config;
use crate::oaita::exec::build_executor;
use crate::oaita::driver::{evaluate_call, generate, run_to_completion, Settings};
use crate::oaita::trace;
use crate::oaita::turns::{append_turn, load_turns, target_segment};

/// Default turn budget granted by `oaita run NAME` when the user
/// omits `--max-steps`. The CLI ALWAYS grants something (vs ask,
/// which defaults to uncapped) so a fresh top-level run has a
/// finite ceiling without the user having to remember the flag.
const DEFAULT_CLI_MAX_STEPS: u32 = 32;

const USAGE: &str = "\
oaita — a resumable OpenAI-compatible chat client (folder-of-turn-files).

USAGE:
  oaita gen   NAME [--model M] [--base-url URL] [--api-key K] [--capabilities T]
  oaita call  NAME [--tool-context N] [--sarun PATH] [--no-sandbox]
  oaita run   NAME [--max-steps N] [...common gen flags + tool-ctx flags]
  oaita tail  NAME
  oaita add   [NAME] [--type ROLE] [--id TURNID] [--from NAME] [--flags F] [--number N]
  oaita trace [ENDPOINT] [--jsonl FILE]
  oaita replay --jsonl FILE [--port N] [--once]
  oaita pretty --jsonl FILE [--session NAME]
  oaita where               (print where oaita.toml is looked up)

NAME may be a dot-stitched spec 'a.b.c' — writes go to the LAST segment;
earlier segments are PREPENDED as context (composition, not hierarchy).

Configuration: {config_home}/oaita.toml — see `oaita where`.";

pub fn main(argv: &[String]) -> i32 {
    let mut it = argv.iter();
    let Some(cmd) = it.next() else { eprintln!("{USAGE}"); return 2; };
    let rest: Vec<String> = it.cloned().collect();
    match cmd.as_str() {
        "gen" => cmd_gen(&rest),
        "call" => cmd_call(&rest),
        "run" => cmd_run(&rest),
        "tail" => cmd_tail(&rest),
        "add" => cmd_add(&rest),
        "trace" => cmd_trace(&rest),
        "replay" => crate::oaita::replay::run(&rest),
        "pretty" => crate::oaita::pretty::run(&rest),
        "where" => cmd_where(),
        "-h" | "--help" => { println!("{USAGE}"); 0 }
        other => { eprintln!("oaita: unknown subcommand {other:?}\n{USAGE}"); 2 }
    }
}

struct Parsed {
    pub name: String,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub capabilities: Option<String>,
    pub tool_context: Option<String>,
    pub sarun: Option<String>,
    pub no_sandbox: bool,
    pub max_steps: Option<u32>,
    pub type_: String,
    pub slug: Option<String>,
    pub sender: Option<String>,
    pub flags: String,
    pub number: Option<u32>,
    pub jsonl: Option<String>,
}

fn parse(args: &[String]) -> Result<Parsed, String> {
    let mut p = Parsed {
        name: String::new(),
        model: None, base_url: None, api_key: None,
        capabilities: None, tool_context: None,
        sarun: None, no_sandbox: false,
        max_steps: None,
        type_: "user".to_string(),
        slug: None, sender: None, flags: String::new(), number: None,
        jsonl: None,
    };
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--model" => p.model = it.next().cloned(),
            "--base-url" => p.base_url = it.next().cloned(),
            "--api-key" => p.api_key = it.next().cloned(),
            "--capabilities" => p.capabilities = it.next().cloned(),
            "--tool-context" => p.tool_context = it.next().cloned(),
            "--sarun" => p.sarun = it.next().cloned(),
            "--no-sandbox" => p.no_sandbox = true,
            "--max-steps" => p.max_steps = Some(it.next()
                .ok_or_else(|| "missing N after --max-steps".to_string())?
                .parse().map_err(|e| format!("--max-steps: {e}"))?),
            "--type" => p.type_ = it.next().cloned()
                .ok_or_else(|| "missing ROLE after --type".to_string())?,
            "--id" => p.slug = it.next().cloned(),
            "--from" => p.sender = it.next().cloned(),
            "--flags" => p.flags = it.next().cloned().unwrap_or_default(),
            "--number" => p.number = it.next()
                .ok_or_else(|| "missing N after --number".to_string())?
                .parse().ok(),
            "--jsonl" => p.jsonl = it.next().cloned(),
            s if !s.starts_with("--") && p.name.is_empty() => p.name = s.to_string(),
            other => return Err(format!("unknown flag {other:?}")),
        }
    }
    Ok(p)
}

fn cmd_gen(args: &[String]) -> i32 {
    let p = match parse(args) { Ok(p) => p, Err(e) => { eprintln!("{e}"); return 2; } };
    if p.name.is_empty() { eprintln!("gen: missing NAME"); return 2; }
    let set = match Settings::resolve(p.model, p.base_url, p.api_key,
                                       p.capabilities, p.tool_context.clone(),
                                       p.sarun.clone(), p.no_sandbox)
    { Ok(s) => s, Err(e) => { eprintln!("{e}"); return 1; } };
    match generate(&p.name, &set) {
        Ok(paths) => report(&paths),
        Err(e) => { eprintln!("oaita: gen: {e}"); 1 }
    }
}

fn cmd_call(args: &[String]) -> i32 {
    let p = match parse(args) { Ok(p) => p, Err(e) => { eprintln!("{e}"); return 2; } };
    if p.name.is_empty() { eprintln!("call: missing NAME"); return 2; }
    let set = match Settings::resolve(p.model, p.base_url, p.api_key,
                                       p.capabilities, p.tool_context.clone(),
                                       p.sarun.clone(), p.no_sandbox)
    { Ok(s) => s, Err(e) => { eprintln!("{e}"); return 1; } };
    let exe = build_executor(p.no_sandbox, p.sarun);
    let exe_ref: Option<&dyn crate::oaita::exec::Executor> = exe.as_deref();
    match evaluate_call(&p.name, &set, exe_ref) {
        Ok(paths) => report(&paths),
        Err(e) => { eprintln!("oaita: call: {e}"); 1 }
    }
}

fn cmd_run(args: &[String]) -> i32 {
    let p = match parse(args) { Ok(p) => p, Err(e) => { eprintln!("{e}"); return 2; } };
    if p.name.is_empty() { eprintln!("run: missing NAME"); return 2; }
    let set = match Settings::resolve(p.model, p.base_url, p.api_key,
                                       p.capabilities, p.tool_context.clone(),
                                       p.sarun.clone(), p.no_sandbox)
    { Ok(s) => s, Err(e) => { eprintln!("{e}"); return 1; } };
    // Set OAITA_SESSION so api.proxy conns the driver opens debit this
    // session's pool. `--max-steps N` is an additive grant — the cli
    // ALWAYS grants (default DEFAULT_CLI_MAX_STEPS if the flag wasn't
    // passed) so a top-level `oaita run NAME` has a finite budget
    // without the user having to remember the flag. ask-level caps
    // are the opposite — uncapped by default — because the model
    // shouldn't have to invent a number for every delegation; the
    // parent's chain is the natural cap.
    let target = match crate::oaita::turns::target_segment(&p.name) {
        Ok(t) => t, Err(e) => { eprintln!("{e}"); return 2; }
    };
    let cli_grant = p.max_steps.unwrap_or(DEFAULT_CLI_MAX_STEPS) as i64;
    if let Err(e) = budget_grant_via_engine(&target, cli_grant) {
        eprintln!("oaita: budget.grant: {e}");
        return 1;
    }
    unsafe { std::env::set_var("OAITA_SESSION", &target); }
    let exe = build_executor(p.no_sandbox, p.sarun);
    let exe_ref: Option<&dyn crate::oaita::exec::Executor> = exe.as_deref();
    match run_to_completion(&p.name, &set, exe_ref) {
        Ok(paths) => report(&paths),
        Err(e) => { eprintln!("oaita: run: {e}"); 1 }
    }
}

/// Send a `budget.grant` RPC to the engine for THIS session. Uses the FD
/// broker when in-box, the host filesystem socket otherwise — same dial
/// dispatch as `oaita::exec::ctrl_rpc`.
fn budget_grant_via_engine(session: &str, amount: i64) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    let mut s = if let Ok(name) = std::env::var("SARUN_BROKER") {
        if !name.is_empty() {
            crate::runner::broker_dial(&name)
                .map_err(|e| format!("broker dial: {e}"))?
        } else {
            UnixStream::connect(crate::paths::sock_path())
                .map_err(|e| format!("connect: {e}"))?
        }
    } else {
        UnixStream::connect(crate::paths::sock_path())
            .map_err(|e| format!("connect: {e}"))?
    };
    let msg = serde_json::json!({
        "type": "budget.grant", "session": session, "amount": amount });
    s.write_all(format!("{msg}\n").as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    let mut line = String::new();
    use std::io::{BufRead, BufReader};
    BufReader::new(&s).read_line(&mut line)
        .map_err(|e| format!("read: {e}"))?;
    Ok(())
}

fn cmd_tail(args: &[String]) -> i32 {
    if args.is_empty() { eprintln!("tail: missing NAME"); return 2; }
    let name = &args[0];
    let target = match target_segment(name) {
        Ok(t) => t, Err(e) => { eprintln!("{e}"); return 2; }
    };
    let turns = load_turns(&target);
    let Some(last) = turns.last() else {
        eprintln!("oaita: no turns in {target}");
        return 1;
    };
    let content = last.read().unwrap_or_default();
    print!("{content}");
    0
}

fn cmd_add(args: &[String]) -> i32 {
    let p = match parse(args) { Ok(p) => p, Err(e) => { eprintln!("{e}"); return 2; } };
    let name = if p.name.is_empty() {
        std::env::var("OAITA_SESSION").unwrap_or_else(|_| "default".to_string())
    } else { p.name };
    let mut content = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut content) {
        eprintln!("oaita: read stdin: {e}");
        return 1;
    }
    match append_turn(&name, &p.type_, &content, p.slug, p.sender, &p.flags, p.number) {
        Ok(path) => { println!("{}", path.display()); 0 }
        Err(e) => { eprintln!("oaita: add: {e}"); 1 }
    }
}

fn cmd_trace(args: &[String]) -> i32 {
    let mut endpoint = "@oaita".to_string();
    let mut jsonl: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--jsonl" => jsonl = it.next().cloned(),
            s if !s.starts_with("--") => endpoint = s.to_string(),
            other => { eprintln!("trace: unknown flag {other:?}"); return 2; }
        }
    }
    trace::run_collector(&endpoint, jsonl.as_deref())
}

fn cmd_where() -> i32 {
    let cfg = crate::paths::oaita_config_path();
    let sock = crate::paths::sock_path();
    let state = crate::paths::oaita_state_home();
    println!("config:        {}", cfg.display());
    println!("control sock:  {} (also carries --api proxy via upgrade)", sock.display());
    println!("sessions root: {}", state.display());
    let c = Config::load();
    println!("model:         {}", c.model.as_deref().unwrap_or("(unset)"));
    println!("base_url:      {}", c.base_url.as_deref().unwrap_or("(unset)"));
    println!("api_key:       {}", if c.api_key.as_deref().unwrap_or("").is_empty() { "(unset)" } else { "***" });
    0
}

fn report(paths: &[std::path::PathBuf]) -> i32 {
    use std::io::Write;
    let _ = writeln!(std::io::stdout(), "");
    for p in paths {
        eprintln!("oaita: wrote {}", p.display());
    }
    0
}
