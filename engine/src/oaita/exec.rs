// Executors — the inward face of the `shell` tool. The SarunExecutor runs the
// script in a persistent sarun BOX (so all writes STAGE for review/apply); the
// LocalExecutor is the ungated subprocess stand-in for tests/no-sarun runs.
//
// The persistent-box naming convention is OAITA-<SESSION> (uppercase prefix
// matches sarun's CLI box-name convention — control::is_box_name checks for an
// uppercase-leading token). One box per session — so the same conversation's
// shell calls compose, just like a long-lived terminal.

use std::process::{Command, Stdio};

use crate::oaita::tools::{ExecResult, summarize_patch, fit_output,
                          RESULT_BUDGET, CHANGES_BUDGET};

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
