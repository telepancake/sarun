// Executors — the inward face of the `shell` tool. The SarunExecutor runs the
// script in a persistent sarun BOX (so all writes STAGE for review/apply); the
// LocalExecutor is the ungated subprocess stand-in for tests/no-sarun runs.
//
// The persistent-box naming convention is OAITA-<SESSION> (uppercase prefix
// matches sarun's CLI box-name convention — control::is_box_name checks for an
// uppercase-leading token). One box per session — so the same conversation's
// shell calls compose, just like a long-lived terminal.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use base64::{Engine as _, prelude::BASE64_STANDARD};
use serde_json::{json, Value};

use crate::oaita::tools::{ExecResult, summarize_patch, fit_output,
                          RESULT_BUDGET, CHANGES_BUDGET};

/// Where the in-box control socket is bind-mounted. Mirrors runner.rs.
const UI_SOCK_INBOX: &str = "/run/sarun/ui.sock";

/// One synchronous request/reply over the engine control socket. In-box if
/// the bind-mounted path exists, host socket otherwise. Returns the unwrapped
/// `r` payload, or an error string.
fn ctrl_rpc(verb: &str, args: Value) -> Result<Value, String> {
    // Prefer the in-box FD broker (abstract UDS served by our parent
    // inner — works in private-netns boxes and leaves no host-path
    // bind-mount inside the box). Falls back to the bind-mounted ui.sock
    // for in-box callers whose parent didn't bring up a broker, then to
    // the host filesystem path.
    let broker = std::env::var("SARUN_BROKER").ok().filter(|s| !s.is_empty());
    let mut s = if let Some(name) = broker.as_ref() {
        match crate::runner::broker_dial(name) {
            Ok(c) => c,
            Err(_) => {
                let sock: PathBuf = if Path::new(UI_SOCK_INBOX).exists() {
                    PathBuf::from(UI_SOCK_INBOX)
                } else {
                    crate::paths::sock_path()
                };
                UnixStream::connect(&sock)
                    .map_err(|e| format!("connect {}: {e}", sock.display()))?
            }
        }
    } else {
        let sock: PathBuf = if Path::new(UI_SOCK_INBOX).exists() {
            PathBuf::from(UI_SOCK_INBOX)
        } else {
            crate::paths::sock_path()
        };
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
    /// Boxes we've materialized so far this process lifetime — once a name
    /// has been ensured we skip the no-op subprocess on subsequent file-IO
    /// touches. Without this, an early `inspect` on a fresh session (before
    /// any `shell` call has created the box) returns "not found" because
    /// resolve(name) fails on the engine side — the box exists in spirit but
    /// not in the box registry until `register` runs. Cheap subprocess once,
    /// then path_kind/read_file/write_file/list_dir hit the engine directly.
    ensured: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl SarunExecutor {
    pub fn new(sarun_override: Option<String>) -> Self {
        let sarun = sarun_override
            .or_else(|| Some(default_sarun()))
            .unwrap();
        SarunExecutor { sarun, ensured: std::sync::Mutex::new(Default::default()) }
    }

    /// First-touch idempotent box materialization: `sarun run BOX -- true`.
    /// Subsequent calls within this process are no-ops (cached by name).
    fn ensure_box(&self, box_id: &str) {
        {
            let g = self.ensured.lock().unwrap();
            if g.contains(box_id) { return; }
        }
        let _ = Command::new(&self.sarun)
            .args(["run", box_id, "--", "true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        self.ensured.lock().unwrap().insert(box_id.to_string());
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
        self.ensure_box(box_id);
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
        self.ensure_box(box_id);
        let rel = to_box_rel(path);
        let b64 = BASE64_STANDARD.encode(bytes);
        ctrl_rpc("box_file_write", json!([box_id, rel, b64]))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        Ok(())
    }

    fn list_dir(&self, box_id: &str, path: &str)
        -> std::io::Result<Vec<(String, char)>>
    {
        self.ensure_box(box_id);
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
        self.ensure_box(box_id);
        let rel = to_box_rel(path);
        match ctrl_rpc("box_path_kind", json!([box_id, rel])) {
            Ok(r) => r.get("kind").and_then(Value::as_str)
                      .and_then(|s| s.chars().next()).unwrap_or('?'),
            Err(_) => '?',
        }
    }

    fn run(&self, box_id: &str, script: &str, discard: bool, api_access: bool) -> ExecResult {
        // For discard mode use a one-shot PEEK box launched as a CHILD of
        // the persistent session box. The dotted name `BOX.PEEK` is sarun's
        // shorthand for "parent=BOX, name=PEEK" — control.rs's register
        // splits the prefix and sets parent_box_id when the parent exists.
        //
        // Why a CHILD instead of a sibling:
        //   sibling PEEK shares only `host` as its lower layer, so writes
        //   the persistent box staged (e.g. /root/fft512.sh) are INVISIBLE
        //   to it — model wrote a file in turn N+1 and `bash that-file`
        //   returned "No such file or directory" in turn N+2.
        //   child PEEK has lower chain `host → persistent → upper`, so it
        //   sees the persistent box's writes AND its own writes are still
        //   discarded after the run.
        let target = if discard {
            // Make sure the persistent parent exists FIRST: dotted names
            // require an extant parent prefix or register errors out.
            let _ = Command::new(&self.sarun)
                .args(["run", box_id, "--", "true"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            format!("{box_id}.PEEK")
        } else {
            box_id.to_string()
        };
        crate::oaita::trace::event("exec.run", serde_json::json!({
            "box": target, "discard": discard, "api_access": api_access,
            "script_len": script.len(),
        }));
        let mut cmd = Command::new(&self.sarun);
        cmd.arg("run");
        if api_access { cmd.arg("--api"); }
        let out = cmd
            .args([&target, "--", "sh", "-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output();
        let (stdout, stderr, rc) = match out {
            Ok(o) => (String::from_utf8_lossy(&o.stdout).into_owned(),
                      filter_sarun_noise(&String::from_utf8_lossy(&o.stderr)),
                      o.status.code().unwrap_or(-1)),
            Err(e) => return ExecResult {
                text: format!("error: cannot run sarun: {e}"),
                rc: -1, ..Default::default()
            }
        };
        let combined = if stderr.is_empty() { stdout.clone() }
                       else { format!("{stdout}{stderr}") };
        // Pull the staged patch via the existing `patch` control verb (the CLI
        // has a `patch` op — see control::cli_box_op).
        let patch = Command::new(&self.sarun)
            .args([&target, "patch"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        let changes = summarize_patch(&patch, CHANGES_BUDGET);
        // Discard mode: tell sarun to throw away the box (no review). Send
        // stdout to /dev/null so the "OAITA-XXX-PEEK: 0 discard" CLI status
        // doesn't leak — the model only ever cares about its script's output.
        if discard {
            let _ = Command::new(&self.sarun).args([&target, "discard"])
                .stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
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
