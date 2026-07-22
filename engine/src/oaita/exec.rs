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
use serde_json::{Value, json};

use crate::oaita::tools::{CHANGES_BUDGET, ExecResult, RESULT_BUDGET, fit_output, summarize_patch};

/// One synchronous request/reply over the engine control socket. The dial
/// path is broker-via-SARUN_BROKER when in-box, filesystem host UDS when
/// not — no fallback chain, no path-presence sniffing.
pub(crate) fn ctrl_rpc(verb: &str, args: Value) -> Result<Value, String> {
    let broker = std::env::var("SARUN_BROKER").ok().filter(|s| !s.is_empty());
    let mut s = if let Some(name) = broker.as_ref() {
        crate::runner::broker_dial(name).map_err(|e| format!("broker dial {name}: {e}"))?
    } else {
        let sock: PathBuf = crate::paths::sock_path();
        UnixStream::connect(&sock).map_err(|e| format!("connect {}: {e}", sock.display()))?
    };
    let msg = json!({"type": "ui", "verb": verb, "args": args});
    s.write_all(format!("{msg}\n").as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    let mut line = String::new();
    BufReader::new(&s)
        .read_line(&mut line)
        .map_err(|e| format!("read: {e}"))?;
    let rep: Value = serde_json::from_str(&line).map_err(|e| format!("parse: {e}"))?;
    if rep.get("ok").and_then(Value::as_bool) != Some(true) {
        return Err(rep
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("rpc failed")
            .to_string());
    }
    Ok(rep.get("r").cloned().unwrap_or(Value::Null))
}

/// Translate a host-style path to a box-relative one. Engine's file ops want
/// rel-to-root (no leading slash), and relative inputs from the model resolve
/// against the runner's cwd.
fn to_box_rel(path: &str) -> String {
    let p = PathBuf::from(path);
    let abs = if p.is_absolute() {
        p
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    };
    abs.strip_prefix("/")
        .map(|q| q.to_string_lossy().into_owned())
        .unwrap_or_else(|_| abs.to_string_lossy().into_owned())
}

pub trait Executor: Send + Sync {
    /// Run a script in a session-owned box, capture stdout+stderr+rc, and
    /// describe the staged changes (`patch_text`-style). `box_id` is the
    /// session-derived box name (UPPERCASE — see `box_name`).
    ///
    /// `api_access` — when true, launch the box with `--api`: an in-box
    /// `oaita run …` resolves (it re-execs /proc/self/exe — see
    /// `default_sarun`), AND the API proxy admits the box. This is what
    /// `act` sub-agents need (they ARE `oaita run` processes in a box).
    /// Plain `shell` tool calls pass false — no need for proxy access on
    /// user scripts.
    fn run(&self, box_id: &str, script: &str, discard: bool, api_access: bool) -> ExecResult;

    /// Read `path` in `box_id`'s merged view. `path` may be absolute or
    /// relative to the runner's cwd; both forms resolve to the same place a
    /// shell-inside-the-box would resolve them. Returns raw bytes.
    fn read_file(&self, box_id: &str, path: &str) -> std::io::Result<Vec<u8>>;

    /// Replace `path` in `box_id`'s upper with `bytes`. Staged exactly like
    /// a shell-inside-the-box write — the host fs is never touched.
    fn write_file(&self, box_id: &str, path: &str, bytes: &[u8]) -> std::io::Result<()>;

    /// Directory listing as (name, kind_char) where kind ∈ 'f'/'d'/'l'/'s'/'?'.
    fn list_dir(&self, box_id: &str, path: &str) -> std::io::Result<Vec<(String, char)>>;

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
        let sarun = sarun_override.or_else(|| Some(default_sarun())).unwrap();
        SarunExecutor { sarun }
    }
}

/// Drop sarun's own status lines from a captured stderr so they don't
/// pollute the tool result that flows back to the model. The runner emits
/// one informational `sarun-engine: box N (overlay root: ...) UI connected`
/// per launch; that's noise to the LLM, never something it can act on.
fn filter_sarun_noise(stderr: &str) -> String {
    stderr
        .lines()
        .filter(|l| !l.starts_with("sarun-engine:"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The engine binary to re-exec for a nested `sarun`/`oaita`.
///
/// In-box (SARUN_BROKER set) there is no `sarun` on PATH and no FUSE shadow:
/// we re-exec the box's inner runner process's own executable, which is the
/// engine binary, via `/proc/self/exe`. Every in-box process descended from
/// the inner is itself the engine binary, so this resolves correctly at any
/// nesting depth without depending on /usr/local or any in-box path.
///
/// On the host: `./sarun` next to a symlinked oaita, else `sarun` on PATH,
/// else current_exe (when invoked via subcommand we already ARE sarun).
fn default_sarun() -> String {
    // In-box: re-exec the engine via the ferried fd (SARUN_EXE), which
    // resolves on ANY rootfs — not `/proc/self/exe`, whose path is absent in a
    // closed OCI rootfs. See runner::in_box_self_exe.
    if in_box() {
        return crate::runner::in_box_self_exe();
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(stem) = exe.file_name().and_then(|s| s.to_str()) {
            if stem == "sarun" || stem == "sarun-engine" {
                return exe.to_string_lossy().into_owned();
            }
        }
        if let Some(parent) = exe.parent() {
            let sib = parent.join("sarun");
            if sib.exists() {
                return sib.to_string_lossy().into_owned();
            }
        }
    }
    "sarun".to_string()
}

/// True when this engine process runs inside a box: the runner sets
/// SARUN_BROKER (the per-box FD-broker abstract-UDS name) on every box child.
pub(crate) fn in_box() -> bool {
    std::env::var("SARUN_BROKER").is_ok_and(|s| !s.is_empty())
}

impl Executor for SarunExecutor {
    fn read_file(&self, box_id: &str, path: &str) -> std::io::Result<Vec<u8>> {
        let rel = to_box_rel(path);
        let r = ctrl_rpc("box_file_read", json!([box_id, rel]))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let b64 = r.get("bytes").and_then(Value::as_str).unwrap_or("");
        BASE64_STANDARD
            .decode(b64)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    fn write_file(&self, box_id: &str, path: &str, bytes: &[u8]) -> std::io::Result<()> {
        let rel = to_box_rel(path);
        let b64 = BASE64_STANDARD.encode(bytes);
        ctrl_rpc("box_file_write", json!([box_id, rel, b64]))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        Ok(())
    }

    fn list_dir(&self, box_id: &str, path: &str) -> std::io::Result<Vec<(String, char)>> {
        let rel = to_box_rel(path);
        let r = ctrl_rpc("box_dir_list", json!([box_id, rel]))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        let arr = r.as_array().cloned().unwrap_or_default();
        Ok(arr
            .into_iter()
            .filter_map(|e| {
                let n = e.get("name").and_then(Value::as_str)?.to_string();
                let k = e
                    .get("kind")
                    .and_then(Value::as_str)?
                    .chars()
                    .next()
                    .unwrap_or('?');
                Some((n, k))
            })
            .collect())
    }

    fn path_kind(&self, box_id: &str, path: &str) -> char {
        let rel = to_box_rel(path);
        match ctrl_rpc("box_path_kind", json!([box_id, rel])) {
            Ok(r) => r
                .get("kind")
                .and_then(Value::as_str)
                .and_then(|s| s.chars().next())
                .unwrap_or('?'),
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
    fn run(&self, _box_id: &str, script: &str, discard: bool, api_access: bool) -> ExecResult {
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
            if api_access {
                cmd.arg("--api");
            }
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
            Ok(o) => (
                String::from_utf8_lossy(&o.stdout).into_owned(),
                filter_sarun_noise(&String::from_utf8_lossy(&o.stderr)),
                o.status.code().unwrap_or(-1),
            ),
            Err(e) => {
                return ExecResult {
                    text: format!("error: cannot run sh: {e}"),
                    rc: -1,
                    ..Default::default()
                };
            }
        };
        let combined = if stderr.is_empty() {
            stdout.clone()
        } else {
            format!("{stdout}{stderr}")
        };
        // Patch summary:
        //   discard: PEEK's own upper (just this script's writes), then reaped.
        //   non-discard: skipped — the wrapper's patch is cumulative across
        //   every shell call this session, so replaying it on each result is
        //   noise that grows monotonically. The model can call inspect/read
        //   if it needs to see the current overlay state.
        let (patch, changes) = if discard {
            let p = Command::new(&self.sarun)
                .args(["PEEK", "patch"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();
            let s = summarize_patch(&p, CHANGES_BUDGET);
            let _ = Command::new(&self.sarun)
                .args(["PEEK", "discard"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
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
        ExecResult {
            text,
            raw_output: combined,
            patch,
            rc,
        }
    }
}

/// Build the executor implied by args. `--no-sandbox` returns None so the
/// shell/inspect/read/write tools surface an error result back to the model
/// instead of dispatching ungated host operations.
pub fn build_executor(
    no_sandbox: bool,
    sarun_override: Option<String>,
) -> Option<Box<dyn Executor>> {
    if no_sandbox {
        return None;
    }
    Some(Box::new(SarunExecutor::new(sarun_override)))
}
