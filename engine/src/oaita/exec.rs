// Executors — the inward face of the `shell` tool. The SarunExecutor runs the
// script in the agent's persistent sarun BOX (so all writes STAGE for review/
// apply); the LocalExecutor is the ungated subprocess stand-in for tests/
// no-sarun runs.
//
// The persistent-box naming convention is OAITA-<SESSION> (uppercase prefix
// matches sarun's CLI box-name convention — control::is_box_name checks for an
// uppercase-leading token). One box per session — so the same conversation's
// shell calls compose, just like a long-lived terminal.
//
// The driver always runs INSIDE that wrapper box (cli::spawn_in_box launches it
// as `sarun run --api OAITA-NAME -- sarun oaita run NAME`). The wrapper IS the
// persistent shell box; shell-tool scripts run as direct `sarun brush-sh sh -c`
// children of this process so their writes accumulate in the wrapper's overlay.
// Discard mode spawns an ephemeral PEEK child (`sarun run -b PEEK -- sh -c
// script`) which sends relname=PEEK — the engine resolves it as a CHILD of
// this wrapper. `sarun PEEK discard` then reaps it.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use base64::{Engine as _, prelude::BASE64_STANDARD};
use serde_json::{json, Value};

use crate::oaita::tools::{ExecResult, summarize_patch, fit_output,
                          RESULT_BUDGET, CHANGES_BUDGET};

/// One synchronous request/reply over the engine control socket. The dial
/// path is broker-via-SARUN_BROKER when in-box, filesystem host UDS when
/// not — no fallback chain, no path-presence sniffing.
fn ctrl_rpc(verb: &str, args: Value) -> Result<Value, String> {
    let broker = std::env::var("SARUN_BROKER").ok().filter(|s| !s.is_empty());
    let mut s = if let Some(name) = broker.as_ref() {
        crate::runner::broker_dial(name)
            .map_err(|e| format!("broker dial {name}: {e}"))?
    } else {
        let sock: PathBuf = crate::paths::sock_path();
        UnixStream::connect(&sock)
            .map_err(|e| format!("connect {}: {e}", sock.display()))?
    };
    let msg = json!({"type": "ui", "verb": verb, "args": args});
    s.write_all(format!("{msg}\n").as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    let mut line = String::new();
    BufReader::new(&s).read_line(&mut line)
        .map_err(|e| format!("read: {e}"))?;
    let rep: Value = serde_json::from_str(&line)
        .map_err(|e| format!("parse: {e}"))?;
    if rep.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(rep.get("error").and_then(Value::as_str)
                   .unwrap_or("rpc failed").to_string());
    }
    Ok(rep.get("r").cloned().unwrap_or(Value::Null))
}

/// Translate a host-style path to a box-relative one. Engine's file ops want
/// rel-to-root (no leading slash), and relative inputs from the model resolve
/// against the runner's cwd.
fn to_box_rel(path: &str) -> String {
    let p = PathBuf::from(path);
    let abs = if p.is_absolute() { p }
              else { std::env::current_dir().unwrap_or_default().join(p) };
    abs.strip_prefix("/").map(|q| q.to_string_lossy().into_owned())
       .unwrap_or_else(|_| abs.to_string_lossy().into_owned())
}

pub trait Executor: Send + Sync {
    /// Run a script in a session-owned box, capture stdout+stderr+rc, and
    /// describe the staged changes (`patch_text`-style). `box_id` is the
    /// session-derived box name (UPPERCASE — see `box_name`).
    ///
    /// `api_access` — when true, launch the box with `--api`: the engine
    /// binary is bound at /usr/local/bin/{oaita,sarun} so an in-box
    /// `oaita run …` resolves, AND the API proxy admits the box. This is
    /// what `act` sub-agents need (they ARE `oaita run` processes in a
    /// box). Plain `shell` tool calls pass false — no need for proxy
    /// access on user scripts.
    fn run(&self, box_id: &str, script: &str, discard: bool, api_access: bool) -> ExecResult;

    /// Read `path` in `box_id`'s merged view. `path` may be absolute or
    /// relative to the runner's cwd; both forms resolve to the same place a
    /// shell-inside-the-box would resolve them. Returns raw bytes.
    fn read_file(&self, box_id: &str, path: &str) -> std::io::Result<Vec<u8>>;

    /// Replace `path` in `box_id`'s upper with `bytes`. Staged exactly like
    /// a shell-inside-the-box write — the host fs is never touched.
    fn write_file(&self, box_id: &str, path: &str, bytes: &[u8])
                  -> std::io::Result<()>;

    /// Directory listing as (name, kind_char) where kind ∈ 'f'/'d'/'l'/'s'/'?'.
    fn list_dir(&self, box_id: &str, path: &str)
                -> std::io::Result<Vec<(String, char)>>;

    /// Kind of `path`: 'f' (file), 'd' (dir), 'l' (symlink), 's' (special),
    /// '?' (absent). A stat-without-error gateway for the inspect tool.
    fn path_kind(&self, box_id: &str, path: &str) -> char;
}

/// The persistent-box name for a session. Capital-letters prefix keeps it
/// distinguishable from human-typed boxes (`MYBOX`) and matches the Python
/// prototype's convention.
pub fn box_name(session: &str) -> String {
    format!("OAITA-{}", session.to_uppercase())
}

pub struct SarunExecutor {
    pub sarun: String,
}

impl SarunExecutor {
    pub fn new(sarun_override: Option<String>) -> Self {
        let sarun = sarun_override
            .or_else(|| Some(default_sarun()))
            .unwrap();
        SarunExecutor { sarun }
    }
}

/// Drop sarun's own status lines from a captured stderr so they don't
/// pollute the tool result that flows back to the model. The runner emits
/// one informational `sarun-engine: box N (overlay root: ...) UI connected`
/// per launch; that's noise to the LLM, never something it can act on.
fn filter_sarun_noise(stderr: &str) -> String {
    stderr.lines()
        .filter(|l| !l.starts_with("sarun-engine:"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Try `./sarun` first (sibling next to a symlinked oaita); else `sarun` on
/// PATH; else current_exe (when invoked via subcommand we already ARE sarun).
fn default_sarun() -> String {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(stem) = exe.file_name().and_then(|s| s.to_str()) {
            if stem == "sarun" || stem == "sarun-engine" {
                return exe.to_string_lossy().into_owned();
            }
        }
        if let Some(parent) = exe.parent() {
            let sib = parent.join("sarun");
            if sib.exists() { return sib.to_string_lossy().into_owned(); }
        }
    }
    "sarun".to_string()
}

impl Executor for SarunExecutor {
    fn read_file(&self, box_id: &str, path: &str) -> std::io::Result<Vec<u8>> {
        let rel = to_box_rel(path);
        let r = ctrl_rpc("box_file_read", json!([box_id, rel]))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let b64 = r.get("bytes").and_then(Value::as_str).unwrap_or("");
        BASE64_STANDARD.decode(b64)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    fn write_file(&self, box_id: &str, path: &str, bytes: &[u8])
        -> std::io::Result<()>
    {
        let rel = to_box_rel(path);
        let b64 = BASE64_STANDARD.encode(bytes);
        ctrl_rpc("box_file_write", json!([box_id, rel, b64]))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        Ok(())
    }

    fn list_dir(&self, box_id: &str, path: &str)
        -> std::io::Result<Vec<(String, char)>>
    {
        let rel = to_box_rel(path);
        let r = ctrl_rpc("box_dir_list", json!([box_id, rel]))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let arr = r.as_array().cloned().unwrap_or_default();
        Ok(arr.into_iter().filter_map(|e| {
            let n = e.get("name").and_then(Value::as_str)?.to_string();
            let k = e.get("kind").and_then(Value::as_str)?
                     .chars().next().unwrap_or('?');
            Some((n, k))
        }).collect())
    }

    fn path_kind(&self, box_id: &str, path: &str) -> char {
        let rel = to_box_rel(path);
        match ctrl_rpc("box_path_kind", json!([box_id, rel])) {
            Ok(r) => r.get("kind").and_then(Value::as_str)
                      .and_then(|s| s.chars().next()).unwrap_or('?'),
            Err(_) => '?',
        }
    }

    /// This process IS the agent's persistent shell box (the wrapper —
    /// `sarun run --api OAITA-<NAME>` was launched by the host CLI
    /// shim, which set OAITA_BOX=1 and re-execed us inside it). For
    /// non-discard calls the script runs DIRECTLY in this wrapper —
    /// `sarun brush-sh sh -c script` — and writes accumulate in the
    /// wrapper's overlay. For discard calls, spawn a `PEEK` child via
    /// `sarun run -b PEEK -- …` (relname → parented to this wrapper)
    /// and discard afterwards.
    fn run(&self, box_id: &str, script: &str, discard: bool, api_access: bool) -> ExecResult {
        let target = if discard { "PEEK".to_string() } else { box_id.to_string() };
        crate::oaita::trace::event("exec.run", serde_json::json!({
            "box": target, "discard": discard, "api_access": api_access,
            "script_len": script.len(),
        }));
        // Discard mode keeps the nested PEEK box (own overlay, then
        // reaped). Non-discard runs DIRECTLY in this wrapper box via
        // the engine binary's `brush-sh` shim — no nested `sarun run`,
        // no extra child box. Either way the script flows through
        // brush-core so its semantic-provenance is visible (FRAME_PROV
        // for the nested case; in-process builtins + per-pipeline
        // execution structure for the direct case).
        let out = if discard {
            let mut cmd = Command::new(&self.sarun);
            cmd.arg("run").arg("-b");
            if api_access { cmd.arg("--api"); }
            cmd.args(["PEEK", "--", "sh", "-c", script])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
        } else {
            Command::new(&self.sarun)
                .args(["brush-sh", "sh", "-c", script])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
        };
        let (stdout, stderr, rc) = match out {
            Ok(o) => (String::from_utf8_lossy(&o.stdout).into_owned(),
                      filter_sarun_noise(&String::from_utf8_lossy(&o.stderr)),
                      o.status.code().unwrap_or(-1)),
            Err(e) => return ExecResult {
                text: format!("error: cannot run sh: {e}"),
                rc: -1, ..Default::default()
            }
        };
        let combined = if stderr.is_empty() { stdout.clone() }
                       else { format!("{stdout}{stderr}") };
        // Patch summary:
        //   discard: PEEK's own upper (just this script's writes), then reaped.
        //   non-discard: skipped — the wrapper's patch is cumulative across
        //   every shell call this session, so replaying it on each result is
        //   noise that grows monotonically. The model can call inspect/read
        //   if it needs to see the current overlay state.
        let (patch, changes) = if discard {
            let p = Command::new(&self.sarun).args(["PEEK", "patch"]).output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();
            let s = summarize_patch(&p, CHANGES_BUDGET);
            let _ = Command::new(&self.sarun).args(["PEEK", "discard"])
                .stdout(Stdio::null()).stderr(Stdio::null()).status();
            (p, s)
        } else {
            (String::new(), String::new())
        };
        let output_budget = RESULT_BUDGET.saturating_sub(changes.len() + 32);
        let trimmed = fit_output(&combined, output_budget);
        let text = if patch.is_empty() {
            trimmed.clone()
        } else {
            format!("{trimmed}\n\n=== changes ===\n{changes}")
        };
        crate::oaita::trace::event("exec.done", serde_json::json!({
            "rc": rc, "bytes": text.len(),
        }));
        ExecResult { text, raw_output: combined, patch, rc }
    }
}

/// Trial-runs / tests: no sarun, no overlay. Writes leak to the host —
/// $OAITA_EXECUTOR=local is opt-in and explicitly NOT a safe substitute.
pub struct LocalExecutor;

impl Executor for LocalExecutor {
    fn read_file(&self, _box_id: &str, path: &str) -> std::io::Result<Vec<u8>> {
        std::fs::read(path)
    }
    fn write_file(&self, _box_id: &str, path: &str, bytes: &[u8])
        -> std::io::Result<()>
    {
        if let Some(parent) = std::path::Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(path, bytes)
    }
    fn list_dir(&self, _box_id: &str, path: &str)
        -> std::io::Result<Vec<(String, char)>>
    {
        let rd = std::fs::read_dir(path)?;
        let mut out = Vec::new();
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let k = e.file_type().ok().map(|t| {
                if t.is_dir() { 'd' }
                else if t.is_symlink() { 'l' }
                else if t.is_file() { 'f' }
                else { 's' }
            }).unwrap_or('?');
            out.push((name, k));
        }
        Ok(out)
    }
    fn path_kind(&self, _box_id: &str, path: &str) -> char {
        match std::fs::symlink_metadata(path) {
            Ok(m) if m.file_type().is_symlink() => 'l',
            Ok(m) if m.is_dir() => 'd',
            Ok(m) if m.is_file() => 'f',
            Ok(_) => 's',
            Err(_) => '?',
        }
    }

    fn run(&self, _box_id: &str, script: &str, _discard: bool, _api_access: bool) -> ExecResult {
        let out = Command::new("sh")
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output();
        let (stdout, stderr, rc) = match out {
            Ok(o) => (String::from_utf8_lossy(&o.stdout).into_owned(),
                      String::from_utf8_lossy(&o.stderr).into_owned(),
                      o.status.code().unwrap_or(-1)),
            Err(e) => return ExecResult {
                text: format!("error: {e}"), rc: -1, ..Default::default()
            },
        };
        let combined = format!("{stdout}{stderr}");
        let trimmed = fit_output(&combined, RESULT_BUDGET);
        ExecResult { text: trimmed.clone(), raw_output: combined, patch: String::new(), rc }
    }
}

/// Build the executor implied by env+args:
///   --no-sandbox          → None (shell calls error-result back)
///   $OAITA_EXECUTOR=local → LocalExecutor (UNGATED)
///   default               → SarunExecutor
pub fn build_executor(no_sandbox: bool, sarun_override: Option<String>)
    -> Option<Box<dyn Executor>>
{
    if no_sandbox { return None; }
    if std::env::var("OAITA_EXECUTOR").as_deref() == Ok("local") {
        return Some(Box::new(LocalExecutor));
    }
    Some(Box::new(SarunExecutor::new(sarun_override)))
}
