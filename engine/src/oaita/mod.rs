// Several helpers below are reachable only from in-box paths the unit tests
// don't drive yet (e.g. trace.rs's TcpLine sink), or from CLI options that
// aren't all exercised in any single run. Silencing dead_code at the module
// root keeps `cargo build --release` clean without scattering allow attrs.
#![allow(dead_code)]

// oaita — the Rust port. A folder-of-turn-files OpenAI-compatible chat client,
// running EITHER as `sarun oaita …` (subcommand) or as a sarun symlinked to
// `oaita` (argv[0] dispatch — same trick brush_sh / n2 / kati use in main.rs).
//
// The Python prototype that this replaces lived at top-level `oaita` on branch
// claude/oaita-cli-mcp-dro88x; design notes in oaita_design.md on that branch.
// The Rust port keeps the same on-disk model (filename grammar, name stitching,
// gen/call/run primitives) but does the wire work with a thin hyper-util HTTP
// client so the SAME client speaks to a real upstream over TLS/TCP AND to the
// engine's `--api` proxy over a unix socket (the no-network-with-API mode).
//
// Submodules:
//   cli      — argparse + dispatch
//   config   — `oaita.toml` (model, base_url, api_key)
//   turns    — filename grammar, parse/load/write
//   ids      — turn-id generation (5 lc letters)
//   client   — minimal OpenAI HTTP/1.1 client; TCP+TLS or UDS transports
//   gen      — `gen` (one model generation, streamed → turn file)
//   call     — `call` (eval one tool call) and `run` (drive to settle)
//   tools    — tool registry (act/shell/inspect/read), schema rendering
//   exec     — executors: SarunExecutor (sarun box -- sh -c), LocalExecutor
//   inspect  — inspect/read helpers
//   trace    — flight-recorder JSONL events to $OAITA_TRACE
//   proxy      — engine-side HTTP server: takes an `api.proxy` conn handed
//                in via the FD broker, injects upstream auth, forwards to
//                the configured LLM API, logs to sqlar

pub mod cli;
pub mod client;
pub mod config;
pub mod driver;
pub mod exec;
pub mod hints;
pub mod ids;
pub mod inspect;
pub mod pretty;
pub mod proxy;
pub mod replay;
pub mod structural;
pub mod tools;
pub mod trace;
pub mod turns;

/// Detect when this binary was invoked as `oaita` (a symlink to the sarun
/// engine binary, mirroring the brush_sh / ninja / make dispatch trick in
/// main.rs). Looks at argv[0]'s basename ONLY — no env var gate, because a
/// symlinked launch outside a sarun box is the supported workflow too.
pub fn is_oaita_invocation() -> bool {
    let Some(arg0) = std::env::args_os().next() else { return false; };
    let p = std::path::Path::new(&arg0);
    let Some(stem) = p.file_name().and_then(|s| s.to_str()) else { return false; };
    stem == "oaita"
}
