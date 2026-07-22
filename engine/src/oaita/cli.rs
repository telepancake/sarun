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
use crate::oaita::driver::{Settings, evaluate_call, generate, run_to_completion};
use crate::oaita::exec::build_executor;
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
  oaita run   NAME [--on BOX] [--task TEXT] [--net MODE] [--max-steps N] [...]
              --on BOX:   run the session's sandbox ON TOP of an existing box
                          (its files are the agent's world; writes layer above)
              --task TEXT: seed NAME with TEXT as the first user turn, then
                          run — one command to pose a task and drive it
              --net MODE: off|tap|host network for the agent's box (default tap)
  oaita tail  NAME
  oaita add   NAME [--type ROLE] [--id TURNID] [--from NAME] [--flags F] [--number N]
  oaita where               (print where oaita.toml is looked up)
  oaita local [--port N] [--setup-only] [--write-config] [...]
              download a tiny tool-capable model + CPU llama.cpp runtime and
              serve it locally — no external endpoint or API key needed

NAME may be a dot-stitched spec 'a.b.c' — writes go to the LAST segment;
earlier segments are PREPENDED as context (composition, not hierarchy).

Internal flags (set by `oaita`'s own re-execs — DO NOT pass by hand):
  --inbox     this process IS the in-box driver; do not wrap.
  --depth N   sub-agent nesting depth (parent's `ask` passes it).

Configuration: {config_home}/oaita.toml — see `oaita where`.";

pub fn main(argv: &[String]) -> i32 {
    let mut it = argv.iter();
    let Some(cmd) = it.next() else {
        eprintln!("{USAGE}");
        return 2;
    };
    let rest: Vec<String> = it.cloned().collect();
    match cmd.as_str() {
        "gen" => cmd_gen(&rest),
        "call" => cmd_call(&rest),
        "run" => cmd_run(&rest),
        "tail" => cmd_tail(&rest),
        "add" => cmd_add(&rest),
        "where" => cmd_where(),
        "local" => crate::oaita::local::cmd_local(&rest),
        "-h" | "--help" => {
            println!("{USAGE}");
            0
        }
        other => {
            eprintln!("oaita: unknown subcommand {other:?}\n{USAGE}");
            2
        }
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
    /// Internal: set by `spawn_in_box` when re-execing into the wrapper
    /// box. Presence means "drive directly"; absence means "host shim,
    /// wrap into a fresh --api box and re-exec".
    pub inbox: bool,
    /// Internal: sub-agent nesting depth. `ask` (driver.rs::act_script)
    /// passes the bumped value when spawning a child.
    pub depth: Option<u32>,
    /// `--net MODE` (off|tap|host): network for the agent's wrapper box.
    /// Forwarded to the outer `sarun run` — WITHOUT it the box always used
    /// the default tap, so a host/off choice from the UI was silently
    /// dropped and a no-netns host got "tap setup failed".
    pub net: Option<String>,
    /// `--task TEXT`: seed the session with TEXT as a user turn before
    /// running (host-side, once) — so a single command both poses the task
    /// and drives it. Lets the UI launch an agent on a box without a
    /// separate `oaita add`. No-op inside the box re-exec (--inbox).
    pub task: Option<String>,
    /// `--on BOX`: parent the session's wrapper box onto this existing box
    /// (dotted session id — same mechanism `oci run` uses to stack a
    /// container on an image). The agent then works on top of that box's
    /// files; its writes layer above them, reviewable like any box.
    pub on: Option<String>,
}

fn parse(args: &[String]) -> Result<Parsed, String> {
    let mut p = Parsed {
        name: String::new(),
        model: None,
        base_url: None,
        api_key: None,
        capabilities: None,
        tool_context: None,
        sarun: None,
        no_sandbox: false,
        max_steps: None,
        type_: "user".to_string(),
        slug: None,
        sender: None,
        flags: String::new(),
        number: None,
        inbox: false,
        depth: None,
        on: None,
        task: None,
        net: None,
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
            "--max-steps" => {
                p.max_steps = Some(
                    it.next()
                        .ok_or_else(|| "missing N after --max-steps".to_string())?
                        .parse()
                        .map_err(|e| format!("--max-steps: {e}"))?,
                )
            }
            "--inbox" => p.inbox = true,
            "--on" => p.on = it.next().cloned(),
            "--task" => p.task = it.next().cloned(),
            "--net" => match it.next().map(String::as_str) {
                Some(m @ ("off" | "tap" | "host")) => p.net = Some(m.to_string()),
                Some(m) => return Err(format!("--net wants off|tap|host, got {m:?}")),
                None => return Err("missing MODE after --net".to_string()),
            },
            "--depth" => {
                p.depth = Some(
                    it.next()
                        .ok_or_else(|| "missing N after --depth".to_string())?
                        .parse()
                        .map_err(|e| format!("--depth: {e}"))?,
                )
            }
            "--type" => {
                p.type_ = it
                    .next()
                    .cloned()
                    .ok_or_else(|| "missing ROLE after --type".to_string())?
            }
            "--id" => p.slug = it.next().cloned(),
            "--from" => p.sender = it.next().cloned(),
            "--flags" => p.flags = it.next().cloned().unwrap_or_default(),
            "--number" => {
                p.number = it
                    .next()
                    .ok_or_else(|| "missing N after --number".to_string())?
                    .parse()
                    .ok()
            }
            s if !s.starts_with("--") && p.name.is_empty() => p.name = s.to_string(),
            other => return Err(format!("unknown flag {other:?}")),
        }
    }
    Ok(p)
}

fn cmd_gen(args: &[String]) -> i32 {
    let p = match parse(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    if p.name.is_empty() {
        eprintln!("gen: missing NAME");
        return 2;
    }
    let set = match Settings::resolve(
        p.model,
        p.base_url,
        p.api_key,
        p.capabilities,
        p.tool_context.clone(),
        p.sarun.clone(),
        p.no_sandbox,
        p.depth,
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    match generate(&p.name, &set) {
        Ok(_) => report(&p.name),
        Err(e) => {
            eprintln!("oaita: gen: {e}");
            1
        }
    }
}

fn cmd_call(args: &[String]) -> i32 {
    let p = match parse(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    if p.name.is_empty() {
        eprintln!("call: missing NAME");
        return 2;
    }
    let set = match Settings::resolve(
        p.model,
        p.base_url,
        p.api_key,
        p.capabilities,
        p.tool_context.clone(),
        p.sarun.clone(),
        p.no_sandbox,
        p.depth,
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let exe = build_executor(p.no_sandbox, p.sarun);
    let exe_ref: Option<&dyn crate::oaita::exec::Executor> = exe.as_deref();
    match evaluate_call(&p.name, &set, exe_ref) {
        Ok(_) => report(&p.name),
        Err(e) => {
            eprintln!("oaita: call: {e}");
            1
        }
    }
}

fn cmd_run(args: &[String]) -> i32 {
    let p = match parse(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    if p.name.is_empty() {
        eprintln!(
            "run: missing NAME (the session/turn-folder to drive; \
                   e.g. `oaita run mytask` — scaffold it with `oaita gen` / \
                   `oaita add`, or pass --task TEXT to seed it inline)"
        );
        return 2;
    }
    // `--inbox` is the explicit marker that we're already running INSIDE
    // the agent's wrapper box (set by spawn_in_box on the re-exec).
    // Without it we're a host shim and must wrap ourselves in a fresh
    // `sarun run --api OAITA-<NAME>` first. Result: there is no host-
    // side oaita driver process; the cli is either a thin spawner
    // (host) or the loop body (in-box). Mirrors `sarun run -- cmd`.
    if !p.inbox {
        // --task seeds the initial user turn HOST-side (once), so a single
        // command both poses the task and drives it. The re-exec below
        // carries --task into the box but the --inbox guard skips re-adding.
        if let Some(task) = &p.task {
            let target = match target_segment(&p.name) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("{e}");
                    return 2;
                }
            };
            if let Err(e) = append_turn(&target, "user", task, None, None, "", None) {
                eprintln!("oaita run: seed --task: {e}");
                return 1;
            }
        }
        return spawn_in_box(&p, args);
    }
    let set = match Settings::resolve(
        p.model,
        p.base_url,
        p.api_key,
        p.capabilities,
        p.tool_context.clone(),
        p.sarun.clone(),
        p.no_sandbox,
        p.depth,
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    // In-box driver: grant our pool from --max-steps (default
    // DEFAULT_CLI_MAX_STEPS). Empty box name → engine resolves identity
    // from the broker hint (THIS box).
    let cli_grant = p
        .max_steps
        .map(|n| n as i64)
        .unwrap_or(DEFAULT_CLI_MAX_STEPS as i64);
    if let Err(e) = budget_grant_via_engine("", cli_grant) {
        eprintln!("oaita: budget.grant: {e}");
        return 1;
    }
    let exe = build_executor(p.no_sandbox, p.sarun);
    let exe_ref: Option<&dyn crate::oaita::exec::Executor> = exe.as_deref();
    match run_to_completion(&p.name, &set, exe_ref) {
        Ok(_) => report(&p.name),
        Err(e) => {
            eprintln!("oaita: run: {e}");
            1
        }
    }
}

/// Host-side `oaita run` shim. Re-execs the SAME oaita command inside a
/// fresh `OAITA-<NAME>` --api box, with `--inbox` added so the inner
/// invocation falls through to the driver loop instead of recursing.
/// The in-box command is `/proc/self/exe` (the box inner runner's own
/// executable — the engine binary the runner exec'd from /proc/self/fd/N),
/// so there is no `sarun`-on-PATH dependency and no FUSE shadow / no
/// /usr/local in the box.
fn spawn_in_box(p: &Parsed, original_args: &[String]) -> i32 {
    let target = match crate::oaita::turns::target_segment(&p.name) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    // `--on BOX` parents the wrapper box onto an existing box via a dotted
    // session id (engine resolves the prefix as the parent — the same
    // stacking `oci run` uses on image layers).
    let target_box = match &p.on {
        Some(parent) => format!("{parent}.{}", crate::oaita::exec::box_name(&target)),
        None => crate::oaita::exec::box_name(&target),
    };
    // Outer `sarun run` runs in the CURRENT context (host, or a parent box):
    // /proc/self/exe in-box, else current_exe(). The `--` payload runs in the
    // NEW box, so it is always /proc/self/exe.
    let exe_path = if crate::oaita::exec::in_box() {
        crate::runner::in_box_self_exe()
    } else {
        std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "sarun".to_string())
    };
    let mut cmd = std::process::Command::new(&exe_path);
    cmd.arg("run");
    // Forward the network mode to the OUTER `sarun run` that builds the box
    // (the inner oaita, under --inbox, spawns nothing). Absent → sarun's own
    // default (tap).
    if let Some(net) = &p.net {
        cmd.arg("--net").arg(net);
    }
    cmd.arg("--api").arg(&target_box).arg("--");
    cmd.arg("/proc/self/exe")
        .arg("oaita")
        .arg("run")
        .arg("--inbox");
    for a in original_args {
        cmd.arg(a);
    }
    match cmd.status() {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("oaita: spawn sarun: {e}");
            1
        }
    }
}

/// Send a `budget.grant` RPC to the engine naming a box (display path).
/// Engine resolves the name to a box_id and writes the new total into
/// the box's sqlar `meta` table.
pub fn budget_grant_via_engine(box_name: &str, amount: i64) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    let mut s = if let Ok(name) = std::env::var("SARUN_BROKER") {
        if !name.is_empty() {
            crate::runner::broker_dial(&name).map_err(|e| format!("broker dial: {e}"))?
        } else {
            UnixStream::connect(crate::paths::sock_path()).map_err(|e| format!("connect: {e}"))?
        }
    } else {
        UnixStream::connect(crate::paths::sock_path()).map_err(|e| format!("connect: {e}"))?
    };
    // Empty box name → engine resolves to the conn's broker hint (the
    // box that dialed). Non-empty → engine looks it up by display path.
    let msg = if box_name.is_empty() {
        serde_json::json!({"type": "budget.grant", "amount": amount })
    } else {
        serde_json::json!({"type": "budget.grant", "box": box_name, "amount": amount })
    };
    s.write_all(format!("{msg}\n").as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    let mut line = String::new();
    use std::io::{BufRead, BufReader};
    BufReader::new(&s)
        .read_line(&mut line)
        .map_err(|e| format!("read: {e}"))?;
    Ok(())
}

fn cmd_tail(args: &[String]) -> i32 {
    if args.is_empty() {
        eprintln!("tail: missing NAME");
        return 2;
    }
    let name = &args[0];
    let target = match target_segment(name) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
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
    let p = match parse(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    if p.name.is_empty() {
        eprintln!("add: missing NAME");
        return 2;
    }
    let name = p.name;
    let mut content = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut content) {
        eprintln!("oaita: read stdin: {e}");
        return 1;
    }
    match append_turn(
        &name, &p.type_, &content, p.slug, p.sender, &p.flags, p.number,
    ) {
        Ok(path) => {
            println!("{}", path.display());
            0
        }
        Err(e) => {
            eprintln!("oaita: add: {e}");
            1
        }
    }
}

fn cmd_where() -> i32 {
    let cfg = crate::paths::oaita_config_path();
    let sock = crate::paths::sock_path();
    let state = crate::paths::oaita_state_home();
    println!("config:        {}", cfg.display());
    println!(
        "control sock:  {} (also carries --api proxy via upgrade)",
        sock.display()
    );
    println!("sessions root: {}", state.display());
    let c = Config::load();
    println!("model:         {}", c.model.as_deref().unwrap_or("(unset)"));
    println!(
        "base_url:      {}",
        c.base_url.as_deref().unwrap_or("(unset)")
    );
    println!(
        "api_key:       {}",
        if c.api_key.as_deref().unwrap_or("").is_empty() {
            "(unset)"
        } else {
            "***"
        }
    );
    0
}

/// Print the session's CURRENT last turn, if it's a clean assistant tail
/// (no `p`/`c`/`b` flags). That's the "final answer" of a settled run —
/// the only thing a programmatic caller (the `ask` tool capturing our
/// stdout) or an interactive user typing `oaita run NAME` actually wants
/// to see. Anything else — per-turn write logs, streamed chunks during
/// gen — would just pollute the caller's view: the turn files on disk
/// are the canonical record, and a quiet stdout makes oaita behave like
/// every other CLI in the shell tool's world (`python script.py` prints
/// its result, not a running commentary).
///
/// We read the session's CURRENT last turn rather than the in-process
/// list of paths we just wrote, because `backtrack(final=true)` rewrites
/// history — the planted summary that IS the answer was written by the
/// rewrite, not returned by `evaluate_call`, so it's invisible to a
/// produced-paths view but visible on disk.
fn report(name: &str) -> i32 {
    let Some(last) = crate::oaita::turns::load_turns(name).into_iter().last() else {
        return 0;
    };
    if last.kind != "assistant" {
        return 0;
    }
    if last.flags.contains('p') || last.flags.contains('c') || last.flags.contains('b') {
        return 0;
    }
    if let Ok(content) = last.read() {
        use std::io::Write;
        let _ = std::io::stdout().write_all(content.as_bytes());
        if !content.ends_with('\n') {
            let _ = writeln!(std::io::stdout());
        }
    }
    0
}
