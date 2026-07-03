//! `oaita local` — a zero-endpoint on-ramp for the Api pane: download a
//! SUPER-TINY tool-capable model (Qwen3-0.6B GGUF by default, ~0.6 GB) plus
//! a CPU-only OpenAI-compatible runtime (llama.cpp's `llama-server`), point
//! oaita.toml at it, and serve on localhost. After this, `oaita gen/run`
//! and the UI's Api view work with NO external endpoint or API key —
//! test-driving and evaluating the agent loop entirely locally.
//!
//! Everything is overridable (--model-url / --runtime-url / --dir / --port)
//! and idempotent: files already present are reused, the config is only
//! overwritten with --write-config (previous one backed up).

use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context, Result};

/// Default model: Qwen3-0.6B, the smallest broadly-available instruct model
/// with real tool-calling support in llama.cpp's chat templates (--jinja).
/// Q4_K_M quant (~378 MiB — roughly half the Q8_0's 610 MiB) from unsloth,
/// which publishes the sub-Q8 quants the official Qwen repo omits.
const DEFAULT_MODEL_URL: &str = "https://huggingface.co/unsloth/Qwen3-0.6B-GGUF/\
                                 resolve/main/Qwen3-0.6B-Q4_K_M.gguf";

/// Latest-release metadata for the CPU runtime; the ubuntu-x64 CPU asset is
/// picked out of it (see pick_runtime_asset).
const RUNTIME_RELEASES_API: &str =
    "https://api.github.com/repos/ggml-org/llama.cpp/releases/latest";

/// Off the beaten path so a dev's own llama-server on 8080 isn't clobbered.
const DEFAULT_PORT: u16 = 18181;

const USAGE: &str = "\
oaita local — download a tiny tool-capable model + CPU runtime and serve it
              locally, so oaita / the Api pane work with no external endpoint.

USAGE:
  oaita local [--port N] [--dir DIR] [--model-url URL] [--runtime-url URL]
              [--setup-only] [--write-config] [--force]

  --port N         listen port for the local server (default 18181)
  --dir DIR        where model+runtime live (default $XDG_DATA_HOME/oaita-local
                   — outside sarun's own dirs, which boxes cannot see)
  --model-url URL  GGUF to fetch (default Qwen3-0.6B-Q4_K_M, ~378 MiB, HF)
  --runtime-url URL  llama.cpp release zip (default: latest CPU ubuntu-x64)
  --setup-only     alias for the default (download only; never serves)
  --write-config   overwrite an existing oaita.toml (backed up to .bak)
  --force          re-download even if files exist
  --no-box         run on the host directly instead of inside sarun boxes
  --net MODE       network for the download box: tap (default) or host

`oaita local` (F4) ONLY DOWNLOADS — it starts no server. The model is
fetched into box 'OAITA-LOCAL' (captured; never applied to the host)
and oaita.toml is pointed at base_url = \"svc://oaita-local#/v1\". That
box then DECLARES an on-demand service: the first time any box calls
the endpoint (and again after every restart), the engine starts the
server as a sub-box PARENTED on OAITA-LOCAL — reading the captured model
with no apply, bridged over the engine's control socket, no host netns.
Nothing is left running to babysit. See engine/DESIGN.md \"On-demand
box services\" — the same mechanism any box can use to advertise a
server. Requires a running engine; --no-box downloads + serves on the
host directly instead.";

pub fn cmd_local(args: &[String]) -> i32 {
    let mut port = DEFAULT_PORT;
    let mut dir: Option<PathBuf> = None;
    let mut model_url = DEFAULT_MODEL_URL.to_string();
    let mut runtime_url: Option<String> = None;
    let mut setup_only = false;
    let mut write_config = false;
    let mut force = false;
    let mut no_box = false;
    let mut inbox = false;
    let mut svc: Option<String> = None;
    let mut net = "tap".to_string();
    let mut net_explicit = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |flag: &str| {
            it.next().cloned().ok_or_else(|| anyhow!("{flag} needs a value"))
        };
        match a.as_str() {
            "--port" => match val("--port").and_then(|v| Ok(v.parse()?)) {
                Ok(p) => port = p,
                Err(e) => { eprintln!("oaita local: --port: {e}"); return 2; }
            },
            "--dir" => match val("--dir") {
                Ok(v) => dir = Some(PathBuf::from(v)),
                Err(e) => { eprintln!("oaita local: {e}"); return 2; }
            },
            "--model-url" => match val("--model-url") {
                Ok(v) => model_url = v,
                Err(e) => { eprintln!("oaita local: {e}"); return 2; }
            },
            "--runtime-url" => match val("--runtime-url") {
                Ok(v) => runtime_url = Some(v),
                Err(e) => { eprintln!("oaita local: {e}"); return 2; }
            },
            "--setup-only" => setup_only = true,
            "--write-config" => write_config = true,
            "--force" => force = true,
            "--no-box" => no_box = true,
            "--net" => match val("--net") {
                Ok(v) if ["tap", "host", "off"].contains(&v.as_str()) => {
                    net = v; net_explicit = true;
                }
                Ok(v) => { eprintln!("oaita local: --net wants tap|host, \
                                      got {v:?}"); return 2; }
                Err(e) => { eprintln!("oaita local: {e}"); return 2; }
            },
            // Internal: set by the box re-exec below — we ARE the in-box
            // payload; do the work directly.
            "--inbox" => inbox = true,
            // Internal: in-box serve should bridge the server out through
            // the engine (svc.serve slots) under this service name.
            "--svc" => match val("--svc") {
                Ok(v) => svc = Some(v),
                Err(e) => { eprintln!("oaita local: {e}"); return 2; }
            },
            "-h" | "--help" => { println!("{USAGE}"); return 0; }
            other => { eprintln!("oaita local: unknown flag {other:?}\n{USAGE}");
                       return 2; }
        }
    }
    let dir = dir.unwrap_or_else(crate::paths::oaita_local_dir);
    // Default: box everything, with the network matched to each phase.
    //
    // DOWNLOAD phase — a box under the DEFAULT tap network: egress goes
    // through the engine's own proxied stack (per-flow policy, flows/pcap
    // visible in the UI), the host netns is never shared, and the fetched
    // bytes are captured in the box for review. Nothing is trusted yet, so
    // nothing gets host access. The phase ends with an explicit apply —
    // 0.6 GB lands on the host only after the user says so.
    //
    // SERVE phase — its own box, and the ONLY reason it shares the host
    // netns (--net host) is inbound reachability: the endpoint must answer
    // on the host's 127.0.0.1 and the tap stack has no inbound path into a
    // box. The server makes no outbound connections; its writes are still
    // captured.
    if !inbox && !no_box {
        if !engine_running() {
            eprintln!("oaita local: no running engine (needed to run the \
                       download in a box) — start `sarun` or `sarun serve`, \
                       or pass --no-box to download onto the host directly");
            return 1;
        }
        // Don't spawn a tap box that can't start: on hosts without netns
        // privileges (unprivileged containers), unshare(CLONE_NEWNET) is
        // denied and the box dies with "tap setup failed".
        let (chosen, note) = resolve_local_net(
            &net, net_explicit, crate::net::tap::tap_available());
        net = chosen;
        if let Some(n) = note { eprintln!("oaita local: {n}"); }
        // Endpoint is ALWAYS the engine-bridged svc socket — regardless of
        // the box's own network. The bridge rides the broker/engine sockets
        // (svc.serve/svc.dial), so it works whether the serve box is tap,
        // off, or host. Uniform endpoint = the engine can recognize it and
        // start it ON DEMAND (control::ensure_service) when a box needs it.
        let base_url = format!("svc://{SVC_NAME}#/v1");
        if let Err(e) = ensure_config(&base_url, write_config) {
            eprintln!("oaita local: {e:#}");
            return 1;
        }
        let this = std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "sarun".into());
        // F4 / `oaita local` ONLY downloads the model — it starts NOTHING.
        // Serving is entirely on demand: the first time a box calls the
        // svc://oaita-local endpoint, the engine starts the serve box
        // (control::ensure_service), and does so again after any restart. So
        // there is no server to babysit and nothing to leave running here.
        //
        // Download (once) into box 'OAITA-LOCAL', captured. A prior download
        // is detected by that box existing, NOT by host files — the files
        // live in the box's overlay and are never applied to the host (the
        // serve box reads them by parenting on OAITA-LOCAL).
        if force || !download_box_ready() {
            let mut cmd = std::process::Command::new(&this);
            cmd.args(["run", "--net", &net, DOWNLOAD_BOX, "--",
                      &this, "oaita", "local", "--inbox", "--setup-only",
                      "--port", &port.to_string(),
                      "--model-url", &model_url,
                      "--dir"]).arg(&dir);
            if let Some(u) = &runtime_url { cmd.args(["--runtime-url", u]); }
            if force { cmd.arg("--force"); }
            eprintln!("oaita local: downloading into box '{DOWNLOAD_BOX}' \
                       (net={net}); the files stay captured in that box — \
                       no host changes");
            match cmd.status() {
                Ok(st) if st.success() => {}
                Ok(st) => return st.code().unwrap_or(1),
                Err(e) => { eprintln!("oaita local: spawn box: {e}"); return 1; }
            }
        } else {
            eprintln!("oaita local: model already downloaded (box \
                       '{DOWNLOAD_BOX}')");
        }
        eprintln!("oaita local: model ready — boxes reach it on demand at \
                   {base_url} (the engine starts the server on the first \
                   call and after restarts; no server left running).");
        return 0;
    }
    match run(&dir, port, &model_url, runtime_url.as_deref(),
              setup_only, write_config && !inbox, force, inbox,
              svc.as_deref()) {
        Ok(()) => 0,
        Err(e) => { eprintln!("oaita local: {e:#}"); 1 }
    }
}

/// The engine-side service name for the bridged endpoint.
const SVC_NAME: &str = "oaita-local";
/// The download box (holds the captured model+runtime) and the serve box
/// (parented on it, runs llama-server + the svc bridge). ALL-CAPS so
/// `sarun <NAME> discard` can address them.
const DOWNLOAD_BOX: &str = "OAITA-LOCAL";
const SERVE_BOX: &str = "OAITA-SERVE";

/// Whether the OAITA-LOCAL download box exists and carries the model — i.e.
/// a prior download succeeded. Checked via the engine (the files live in
/// that box's overlay, not on the host). Absent box → need to download.
fn download_box_ready() -> bool {
    let boxes = crate::discover::discover();
    let Some((id, _)) = boxes.iter().find(|(_, b)| b.name == DOWNLOAD_BOX)
        else { return false };
    // The box exists; trust that a successful --setup-only left the files.
    // (A partial download leaves the box too, but `--force` re-runs it.)
    let _ = id;
    true
}

/// Decide the box network mode, given the requested mode, whether the user
/// forced it, and whether tap can actually work here. When the default tap
/// can't run (no CLONE_NEWNET), fall back to host and return a note — the
/// download is still captured, only network isolation is lost. An explicit
/// choice is always respected (returned as-is, left to fail loudly if the
/// user forced tap on a host that can't do it).
fn resolve_local_net(net: &str, explicit: bool, tap_ok: bool)
    -> (String, Option<String>)
{
    if net == "tap" && !explicit && !tap_ok {
        return ("host".to_string(), Some(
            "tap networking unavailable here (no CLONE_NEWNET) — using \
             --net host for the box (writes are still captured; pass \
             --net off to air-gap, or --net tap to force)".to_string()));
    }
    (net.to_string(), None)
}

/// Whether the engine's control socket answers (a box run needs it).
fn engine_running() -> bool {
    std::os::unix::net::UnixStream::connect(crate::paths::sock_path()).is_ok()
}

/// The first `*.gguf` model in `dir` (the serve path locates the model by
/// scanning, so it needs no --model-url — the download named the file).
fn find_gguf(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?
        .filter_map(|e| e.ok()).map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "gguf"))
}

/// Declare (from inside the download box) that THIS box provides the
/// on-demand `oaita-local` service: the engine stamps the declaration onto
/// this box's meta, so a later svc://oaita-local call starts the serve
/// payload as a sub-box parented here. See control::ensure_service.
fn declare_service(dir: &Path) {
    let Ok(broker) = std::env::var("SARUN_BROKER") else { return };
    if broker.is_empty() { return; }
    // Payload run in the serve sub-box: serve THIS dir over the svc bridge.
    let argv = serde_json::json!([
        "/proc/self/exe", "oaita", "local", "--inbox",
        "--svc", SVC_NAME, "--dir", dir.to_string_lossy()]);
    let msg = serde_json::json!({
        "type": "svc.declare", "name": SVC_NAME, "argv": argv, "net": ""});
    if let Ok(mut c) = crate::runner::broker_dial(&broker) {
        let _ = c.write_all(format!("{msg}\n").as_bytes());
        let mut line = String::new();
        let _ = std::io::BufRead::read_line(
            &mut std::io::BufReader::new(&c), &mut line);
    }
}

fn run(dir: &Path, port: u16, model_url: &str, runtime_url: Option<&str>,
       setup_only: bool, write_config: bool, force: bool, inbox: bool,
       svc: Option<&str>) -> Result<()> {
    // Tolerate a raw EEXIST: under the box's FUSE overlay, mkdir of a dir
    // that exists host-side can report EEXIST while the follow-up stat that
    // std's create_dir_all uses to excuse it doesn't line up.
    match std::fs::create_dir_all(dir) {
        Ok(()) => {}
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {}
        Err(e) => return Err(e).with_context(|| format!("mkdir {dir:?}")),
    }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()
        .context("tokio runtime")?;
    let server = dir.join("llama-server");

    // DOWNLOAD when setting up, or (host `--no-box`) when the files aren't
    // there yet. The SERVE path never downloads — it locates the existing
    // model by scanning, so it needs no URL.
    let need_download = force || setup_only
        || find_gguf(dir).is_none() || !server.is_file();
    if need_download {
        let model_name = url_filename(model_url)
            .ok_or_else(|| anyhow!("cannot derive a filename from {model_url}"))?;
        let model_path = dir.join(&model_name);
        if force || !model_path.is_file() {
            rt.block_on(fetch_to(model_url, &model_path, "model"))?;
        } else {
            eprintln!("model already present: {}", model_path.display());
        }
        if force || !server.is_file() {
            let url = match runtime_url {
                Some(u) => u.to_string(),
                None => rt.block_on(latest_runtime_url())?,
            };
            let arch_name = url_filename(&url)
                .unwrap_or_else(|| "runtime.tar.gz".into());
            let arch_path = dir.join(&arch_name);
            rt.block_on(fetch_to(&url, &arch_path, "runtime"))?;
            let n = extract_runtime(&arch_path, dir)?;
            let _ = std::fs::remove_file(&arch_path);
            if !server.is_file() {
                bail!("archive extracted ({n} files) but no llama-server in \
                       it — pass --runtime-url with a llama.cpp \
                       *bin-ubuntu-x64* archive, or drop a llama-server \
                       binary into {}", dir.display());
            }
            eprintln!("runtime ready: {} ({n} files)", server.display());
        }
    }

    // Host-side `--no-box` writes its own (plain http) config; boxed flows
    // get the svc:// config from cmd_local.
    if !inbox {
        ensure_config(&format!("http://127.0.0.1:{port}/v1"), write_config)?;
    }

    if setup_only {
        // Advertise the on-demand service so the engine can start it later.
        declare_service(dir);
        eprintln!("setup complete — model captured; the engine serves it on \
                   demand");
        return Ok(());
    }

    // SERVE: locate the model by scan (no URL needed) and run.
    let model_path = find_gguf(dir)
        .ok_or_else(|| anyhow!("no *.gguf model in {} — run `oaita local` \
            (F4) to download it first", dir.display()))?;
    if !server.is_file() {
        bail!("no llama-server in {} — run `oaita local` to fetch the runtime",
              dir.display());
    }
    serve(dir, &model_path, port, svc)
}

/// GET `url` streaming to `dest` (via .part + rename), with coarse progress
/// on stderr. Follows redirects (Hugging Face resolves to a CDN).
/// HTTP client for the downloads. On top of the compiled-in webpki roots,
/// trust the SYSTEM CA bundle when one is present: inside a tap-net box the
/// engine MITMs HTTPS with a CA it injects into the box's bundle — without
/// this the boxed download would fail TLS (rustls ships its own roots and
/// ignores /etc/ssl). Also honors SSL_CERT_FILE.
fn http_client() -> Result<reqwest::Client> {
    let mut b = reqwest::Client::builder().user_agent("sarun-oaita-local");
    let mut bundles: Vec<PathBuf> = vec![
        "/etc/ssl/certs/ca-certificates.crt".into(),
        "/etc/pki/tls/certs/ca-bundle.crt".into(),
    ];
    if let Ok(p) = std::env::var("SSL_CERT_FILE") {
        if !p.is_empty() { bundles.insert(0, p.into()); }
    }
    for p in bundles {
        let Ok(bytes) = std::fs::read(&p) else { continue };
        if let Ok(certs) = reqwest::Certificate::from_pem_bundle(&bytes) {
            for c in certs { b = b.add_root_certificate(c); }
        }
        break; // first readable bundle wins (they're alternatives)
    }
    b.build().context("http client")
}

async fn fetch_to(url: &str, dest: &Path, label: &str) -> Result<()> {
    eprintln!("downloading {label}: {url}");
    let client = http_client()?;
    let mut resp = client.get(url).send().await
        .with_context(|| format!("GET {url}"))?
        .error_for_status()
        .with_context(|| format!("GET {url}"))?;
    let total = resp.content_length();
    let part = dest.with_extension("part");
    let mut out = std::fs::File::create(&part)
        .with_context(|| format!("create {part:?}"))?;
    let mut got: u64 = 0;
    let mut last_mark: u64 = 0;
    while let Some(chunk) = resp.chunk().await.context("read body")? {
        out.write_all(&chunk).context("write")?;
        got += chunk.len() as u64;
        if got - last_mark >= 64 * 1024 * 1024 {
            last_mark = got;
            match total {
                Some(t) => eprintln!("  {label}: {} / {} MiB",
                                     got >> 20, t >> 20),
                None => eprintln!("  {label}: {} MiB", got >> 20),
            }
        }
    }
    out.flush().ok();
    drop(out);
    std::fs::rename(&part, dest).with_context(|| format!("rename to {dest:?}"))?;
    eprintln!("  {label}: done ({} MiB) → {}", got >> 20, dest.display());
    Ok(())
}

/// Resolve the latest llama.cpp CPU runtime zip for this platform from the
/// GitHub releases API.
async fn latest_runtime_url() -> Result<String> {
    let client = http_client()?;
    let body = client.get(RUNTIME_RELEASES_API).send().await
        .context("GET releases")?
        .error_for_status().context("GET releases")?
        .text().await.context("read releases")?;
    let v: serde_json::Value = serde_json::from_str(&body)
        .context("parse releases JSON")?;
    pick_runtime_asset(&v)
        .ok_or_else(|| anyhow!("no CPU ubuntu-x64 asset in the latest \
            llama.cpp release — pass --runtime-url explicitly"))
}

/// Pick the plain-CPU linux/x64 archive out of a llama.cpp release's asset
/// list — e.g. `llama-b9860-bin-ubuntu-x64.tar.gz` (older releases shipped
/// .zip; both are accepted) — skipping every accelerator/other-arch build.
fn pick_runtime_asset(release: &serde_json::Value) -> Option<String> {
    let assets = release.get("assets")?.as_array()?;
    let bad = ["cuda", "vulkan", "sycl", "hip", "rocm", "arm64", "s390x",
               "musa", "kompute", "openvino", "opencl", "android", "win",
               "macos", "xcframework", "cudart", "-ui."];
    assets.iter().find_map(|a| {
        let name = a.get("name")?.as_str()?.to_ascii_lowercase();
        let linux_x64 = (name.contains("ubuntu") || name.contains("linux"))
            && name.contains("x64")
            && (name.ends_with(".zip") || name.ends_with(".tar.gz")
                || name.ends_with(".tgz"));
        if linux_x64 && !bad.iter().any(|b| name.contains(b)) {
            a.get("browser_download_url")?.as_str().map(str::to_string)
        } else {
            None
        }
    })
}

/// The payload worth keeping from a runtime archive: the server + every
/// shared lib it links; demos/tools/docs are skipped.
fn keep_runtime_file(name: &str) -> bool {
    name == "llama-server" || (name.starts_with("lib") && name.contains(".so"))
}

fn install_runtime_file(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let dest = dir.join(name);
    std::fs::write(&dest, bytes).with_context(|| format!("write {dest:?}"))?;
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest,
            std::fs::Permissions::from_mode(0o755)).ok();
    }
    Ok(())
}

/// Basename of an archive path.
fn base_of(p: &str) -> String {
    Path::new(p).file_name().and_then(|s| s.to_str())
        .unwrap_or("").to_string()
}

/// Recreate a (flattened) symlink `name -> target-basename` in `dir`. The
/// SONAME chain a llama.cpp release ships (`libfoo.so.0 -> libfoo.so.0.1.2`)
/// is same-directory relative, so flattening keeps it valid.
fn install_symlink(dir: &Path, name: &str, target: &str) -> Result<()> {
    let p = dir.join(name);
    let _ = std::fs::remove_file(&p);
    std::os::unix::fs::symlink(base_of(target), &p)
        .with_context(|| format!("symlink {name} -> {target}"))?;
    Ok(())
}

/// Flatten the useful payload of a llama.cpp release archive (the server
/// binary + its shared libs live under build/bin/) into `dir`. Handles the
/// current .tar.gz assets and the older .zip ones — AND the SONAME symlinks
/// (`libfoo.so.0 -> libfoo.so.0.1.2`): the binary's DT_NEEDED names the
/// symlink, so dropping it (as "not a regular file") is exactly why
/// llama-server failed with `libllama-common.so.0: cannot open shared
/// object file`. Returns entries written.
fn extract_runtime(archive: &Path, dir: &Path) -> Result<usize> {
    let is_zip = archive.extension().is_some_and(|e| e == "zip");
    let mut n = 0;
    if is_zip {
        let f = std::fs::File::open(archive).context("open runtime archive")?;
        let mut z = zip::ZipArchive::new(f).context("read runtime zip")?;
        for i in 0..z.len() {
            let mut e = z.by_index(i).context("zip entry")?;
            if e.is_dir() { continue; }
            let name = base_of(e.name());
            if !keep_runtime_file(&name) { continue; }
            let is_link = e.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000);
            let mut buf = Vec::new();
            e.read_to_end(&mut buf).context("read zip entry")?;
            if is_link {
                install_symlink(dir, &name, &String::from_utf8_lossy(&buf))?;
            } else {
                install_runtime_file(dir, &name, &buf)?;
            }
            n += 1;
        }
    } else {
        let f = std::fs::File::open(archive).context("open runtime archive")?;
        let gz = flate2::read::GzDecoder::new(f);
        let mut t = tar::Archive::new(gz);
        for e in t.entries().context("read runtime tar")? {
            let mut e = e.context("tar entry")?;
            let et = e.header().entry_type();
            let name = e.path().ok()
                .and_then(|p| p.file_name()
                    .and_then(|s| s.to_str()).map(str::to_string))
                .unwrap_or_default();
            if !keep_runtime_file(&name) { continue; }
            if et.is_symlink() {
                if let Ok(Some(link)) = e.link_name() {
                    install_symlink(dir, &name, &link.to_string_lossy())?;
                    n += 1;
                }
            } else if et.is_file() {
                let mut buf = Vec::new();
                e.read_to_end(&mut buf).context("read tar entry")?;
                install_runtime_file(dir, &name, &buf)?;
                n += 1;
            }
        }
    }
    Ok(n)
}

/// The oaita.toml contents pointing at the local server.
fn local_config(base_url: &str) -> String {
    format!("# written by `oaita local` — a llama.cpp server on this machine.\n\
             model = \"local\"\n\
             base_url = \"{base_url}\"\n\
             api_key = \"sk-local\"\n")
}

/// Write oaita.toml for the local endpoint. An existing, USABLE config
/// (one that actually sets a model) is left alone unless --write-config.
/// A config with no model is broken — the agent box would hit "no model
/// set" — so it is replaced (backed up first) even without --write-config,
/// so F4 always leaves a working config.
fn ensure_config(base_url: &str, overwrite: bool) -> Result<()> {
    let p = crate::paths::oaita_config_path();
    if p.is_file() && !overwrite {
        let has_model = crate::oaita::config::Config::load_from(&p)
            .model.map(|m| !m.trim().is_empty()).unwrap_or(false);
        if has_model {
            eprintln!("keeping existing {} (use --write-config to point it \
                       at the local server)", p.display());
            return Ok(());
        }
        eprintln!("existing {} sets no model — replacing it with the local \
                   config (the old one is backed up)", p.display());
        // fall through to write (with backup)
    }
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {parent:?}"))?;
    }
    if p.is_file() {
        let bak = p.with_extension("toml.bak");
        std::fs::copy(&p, &bak).with_context(|| format!("backup to {bak:?}"))?;
        eprintln!("backed up existing config to {}", bak.display());
    }
    std::fs::write(&p, local_config(base_url))
        .with_context(|| format!("write {p:?}"))?;
    eprintln!("wrote {}", p.display());
    Ok(())
}

/// One svc.serve accept slot: park a broker connection with the engine,
/// wait for a host client to be paired onto it, then splice it to the
/// in-box llama-server over box-local loopback. Loops forever (each thread
/// serves streams sequentially; run a few for concurrency).
fn svc_bridge_loop(broker: String, name: String, port: u16) {
    loop {
        let Ok(mut conn) = crate::runner::broker_dial(&broker) else {
            std::thread::sleep(std::time::Duration::from_secs(2));
            continue;
        };
        if conn.write_all(
            format!("{{\"type\":\"svc.serve\",\"name\":\"{name}\"}}\n")
                .as_bytes()).is_err()
        {
            std::thread::sleep(std::time::Duration::from_secs(2));
            continue;
        }
        // Two engine lines: "parked" now, "paired" when a host client
        // arrives. Byte-wise reads — anything after the second newline is
        // already the spliced stream's payload and must not be swallowed
        // by a buffered reader.
        if read_line_bytewise(&mut conn).is_none() { continue; }
        if read_line_bytewise(&mut conn).is_none() { continue; }
        let Ok(srv) = std::net::TcpStream::connect(("127.0.0.1", port)) else {
            // Server gone mid-flight; drop the paired stream (host client
            // sees EOF) and retry parking.
            continue;
        };
        splice_streams(conn, srv);
    }
}

fn read_line_bytewise(s: &mut impl Read) -> Option<String> {
    let mut out = Vec::new();
    let mut b = [0u8; 1];
    loop {
        match s.read(&mut b) {
            Ok(0) | Err(_) => return None,
            Ok(_) if b[0] == b'\n' => break,
            Ok(_) => out.push(b[0]),
        }
    }
    String::from_utf8(out).ok()
}

/// Splice an engine (Unix) stream and the local TCP server stream both
/// ways; returns when both directions are done so the slot can re-park.
fn splice_streams(uds: std::os::unix::net::UnixStream,
                  tcp: std::net::TcpStream) {
    let (Ok(mut ur), Ok(mut tw)) = (uds.try_clone(), tcp.try_clone()) else { return };
    let t = std::thread::spawn(move || {
        let _ = std::io::copy(&mut ur, &mut tw);
        let _ = tw.shutdown(std::net::Shutdown::Write);
    });
    let (mut tr, mut uw) = (tcp, uds);
    let _ = std::io::copy(&mut tr, &mut uw);
    let _ = uw.shutdown(std::net::Shutdown::Write);
    let _ = t.join();
}

/// Run llama-server in the foreground on `port`, announce readiness once
/// /health answers, and wait until it exits (Ctrl-C). With `svc`, also
/// bridge the endpoint out through the engine (svc.serve slots) so the
/// host reaches it over a UDS while this box keeps its isolated netns.
fn serve(dir: &Path, model: &Path, port: u16, svc: Option<&str>) -> Result<()> {
    let server = dir.join("llama-server");
    eprintln!("starting {} on 127.0.0.1:{port} …", server.display());
    let mut child = std::process::Command::new(&server)
        .arg("-m").arg(model)
        // 32k context: the oaita agent harness prompt + tool schemas alone
        // is ~4k tokens, and tool-result turns accumulate — 8k overflowed
        // after a couple of steps (llama.cpp then errors mid-stream). Qwen3
        // handles 32k natively; the KV cache for a 0.6B model is modest.
        .args(["--host", "127.0.0.1", "--port", &port.to_string(),
               "--jinja", "-c", "32768"])
        // extracted shared libs sit next to the binary
        .env("LD_LIBRARY_PATH", dir)
        .spawn()
        .with_context(|| format!(
            "spawn {} — if this is an exec/loader error the prebuilt \
             runtime doesn't run on this host (it links glibc); pass \
             --runtime-url with a build for your platform or drop your own \
             llama-server into {}", server.display(), dir.display()))?;
    // readiness probe: llama-server serves /health once the model is loaded.
    let url = format!("http://127.0.0.1:{port}/health");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    loop {
        if let Some(st) = child.try_wait().context("child status")? {
            bail!("llama-server exited during startup ({st}) — see its \
                   output above; a loader/glibc error means the prebuilt \
                   runtime doesn't run here (see --runtime-url)");
        }
        if std::time::Instant::now() > deadline { break; }
        if health_ok(&url) {
            if let Some(name) = svc {
                let Ok(broker) = std::env::var("SARUN_BROKER") else {
                    bail!("--svc given but SARUN_BROKER is not set — \
                           the bridge only works from inside a box");
                };
                for _ in 0..4 {
                    let (b, n) = (broker.clone(), name.to_string());
                    std::thread::spawn(move || svc_bridge_loop(b, n, port));
                }
            }
            eprintln!();
            match svc {
                Some(name) => eprintln!(
                    "ready — endpoint bridged through the engine as \
                     svc://{name} (this box's network stays isolated)"),
                None => eprintln!(
                    "ready — local OpenAI-compatible endpoint: \
                     http://127.0.0.1:{port}/v1"),
            }
            eprintln!("try:   sarun oaita gen demo   (then `sarun oaita add \
                       demo`, `sarun oaita run demo`)");
            eprintln!("the UI's Api pane shows the traffic; Ctrl-C here \
                       stops the server.");
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let st = child.wait().context("wait llama-server")?;
    if !st.success() { bail!("llama-server exited: {st}"); }
    Ok(())
}

/// Plain blocking TCP one-liner so the poll loop needs no async plumbing.
fn health_ok(url: &str) -> bool {
    let Some(hostport) = url.strip_prefix("http://")
        .and_then(|r| r.split('/').next()) else { return false };
    let Ok(mut s) = std::net::TcpStream::connect(hostport) else { return false };
    let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(2)));
    if s.write_all(b"GET /health HTTP/1.0\r\n\r\n").is_err() { return false; }
    let mut buf = [0u8; 64];
    match s.read(&mut buf) {
        Ok(n) => String::from_utf8_lossy(&buf[..n]).contains(" 200"),
        Err(_) => false,
    }
}

fn url_filename(url: &str) -> Option<String> {
    url.split('/').next_back()
        .map(|s| s.split('?').next().unwrap_or(s))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_picker_finds_the_cpu_asset_in_a_real_release() {
        // the VERBATIM asset list of llama.cpp release b9860 (tar.gz era) —
        // the picker must land on plain bin-ubuntu-x64 and nothing else.
        let names = [
            "llama-b9860-bin-macos-arm64.tar.gz",
            "llama-b9860-bin-macos-x64.tar.gz",
            "llama-b9860-xcframework.zip",
            "llama-b9860-bin-ubuntu-x64.tar.gz",
            "llama-b9860-bin-ubuntu-arm64.tar.gz",
            "llama-b9860-bin-ubuntu-s390x.tar.gz",
            "llama-b9860-bin-ubuntu-vulkan-x64.tar.gz",
            "llama-b9860-bin-ubuntu-vulkan-arm64.tar.gz",
            "llama-b9860-bin-ubuntu-rocm-7.2-x64.tar.gz",
            "llama-b9860-bin-ubuntu-openvino-2026.2.1-x64.tar.gz",
            "llama-b9860-bin-ubuntu-sycl-fp32-x64.tar.gz",
            "llama-b9860-bin-ubuntu-sycl-fp16-x64.tar.gz",
            "llama-b9860-bin-android-arm64.tar.gz",
            "llama-b9860-bin-win-cpu-x64.zip",
            "llama-b9860-bin-win-cuda-12.4-x64.zip",
            "cudart-llama-bin-win-cuda-12.4-x64.zip",
            "llama-b9860-ui.tar.gz",
        ];
        let assets: Vec<serde_json::Value> = names.iter().map(|n|
            serde_json::json!({"name": n,
                               "browser_download_url": format!("https://x/{n}")}))
            .collect();
        let rel = serde_json::json!({"assets": assets});
        assert_eq!(pick_runtime_asset(&rel).as_deref(),
                   Some("https://x/llama-b9860-bin-ubuntu-x64.tar.gz"));
        // older zip-era releases still resolve
        let rel = serde_json::json!({"assets": [
            {"name": "llama-b4000-bin-ubuntu-x64.zip",
             "browser_download_url": "https://x/cpu.zip"}]});
        assert_eq!(pick_runtime_asset(&rel).as_deref(), Some("https://x/cpu.zip"));
        let none: serde_json::Value = serde_json::json!({"assets": []});
        assert!(pick_runtime_asset(&none).is_none());
    }

    #[test]
    fn runtime_extraction_handles_tar_gz() {
        use std::io::Write as _;
        let tmp = std::env::temp_dir()
            .join(format!("oaita-local-tgz-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let tp = tmp.join("rt.tar.gz");
        {
            let f = std::fs::File::create(&tp).unwrap();
            let gz = flate2::write::GzEncoder::new(f, Default::default());
            let mut t = tar::Builder::new(gz);
            for (name, body) in [
                ("build/bin/llama-server", b"ELF".as_slice()),
                // real SONAME file + the versioned symlink the binary links
                // against (libllama-common.so.0), which used to be dropped.
                ("build/bin/libllama-common.so.0.1.2", b"ELF".as_slice()),
                ("build/bin/llama-cli", b"ELF".as_slice()),
                ("README.md", b"docs".as_slice()),
            ] {
                let mut h = tar::Header::new_gnu();
                h.set_size(body.len() as u64);
                h.set_mode(0o755);
                h.set_cksum();
                t.append_data(&mut h, name, body).unwrap();
            }
            // the SONAME symlink: libllama-common.so.0 -> ...so.0.1.2
            let mut hl = tar::Header::new_gnu();
            hl.set_entry_type(tar::EntryType::Symlink);
            hl.set_size(0);
            hl.set_mode(0o777);
            t.append_link(&mut hl, "build/bin/libllama-common.so.0",
                          "libllama-common.so.0.1.2").unwrap();
            t.into_inner().unwrap().finish().unwrap().flush().unwrap();
        }
        let out = tmp.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let n = extract_runtime(&tp, &out).unwrap();
        assert_eq!(n, 3, "server + real lib + SONAME symlink; no cli/docs");
        assert!(out.join("llama-server").is_file());
        assert!(out.join("libllama-common.so.0.1.2").is_file());
        assert!(!out.join("llama-cli").exists());
        // THE fix: the versioned SONAME symlink is preserved and resolves,
        // so the loader finds libllama-common.so.0.
        let link = out.join("libllama-common.so.0");
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink(),
                "SONAME must be a symlink");
        assert!(std::fs::read(&link).is_ok(),
                "SONAME symlink must resolve to the real lib");
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn runtime_extraction_flattens_server_and_libs_only() {
        use std::io::Write as _;
        let tmp = std::env::temp_dir()
            .join(format!("oaita-local-zip-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let zp = tmp.join("rt.zip");
        {
            let f = std::fs::File::create(&zp).unwrap();
            let mut w = zip::ZipWriter::new(f);
            let o: zip::write::SimpleFileOptions = Default::default();
            for (name, body) in [
                ("build/bin/llama-server", b"ELF".as_slice()),
                ("build/bin/libllama.so", b"ELF".as_slice()),
                ("build/bin/libggml.so.1", b"ELF".as_slice()),
                ("build/bin/llama-quantize", b"ELF".as_slice()),
                ("README.md", b"docs".as_slice()),
            ] {
                w.start_file(name, o).unwrap();
                w.write_all(body).unwrap();
            }
            w.finish().unwrap();
        }
        let out = tmp.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let n = extract_runtime(&zp, &out).unwrap();
        assert_eq!(n, 3, "server + 2 libs, no extra tools/docs");
        assert!(out.join("llama-server").is_file());
        assert!(out.join("libllama.so").is_file());
        assert!(out.join("libggml.so.1").is_file());
        assert!(!out.join("llama-quantize").exists());
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(out.join("llama-server"))
                .unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "server must be executable");
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn local_config_points_oaita_at_the_local_server() {
        let c = local_config("http://127.0.0.1:18181/v1");
        assert!(c.contains("base_url = \"http://127.0.0.1:18181/v1\""));
        assert!(c.contains("model = \"local\""));
        // parses as the Config the rest of oaita reads
        let v: toml::Value = c.parse().unwrap();
        assert_eq!(v.get("api_key").and_then(|x| x.as_str()), Some("sk-local"));
        // the bridged (isolated-box) endpoint form parses as a Svc endpoint
        // with the base path in the fragment
        let c = local_config("svc://oaita-local#/v1");
        let v: toml::Value = c.parse().unwrap();
        let url = v.get("base_url").and_then(|x| x.as_str()).unwrap();
        match crate::oaita::client::Endpoint::parse_url(url).unwrap() {
            crate::oaita::client::Endpoint::Svc { name } =>
                assert_eq!(name, "oaita-local"),
            other => panic!("expected Svc endpoint, got {other:?}"),
        }
    }

    #[test]
    fn local_net_falls_back_to_host_when_tap_dead() {
        // default tap on a no-netns host → host, with an explanation
        let (net, note) = resolve_local_net("tap", false, false);
        assert_eq!(net, "host");
        assert!(note.unwrap().contains("CLONE_NEWNET"));
        // tap available → stays tap, silent
        assert_eq!(resolve_local_net("tap", false, true), ("tap".into(), None));
        // explicit --net tap is respected even if it'll fail (no silent
        // downgrade of an explicit choice)
        assert_eq!(resolve_local_net("tap", true, false), ("tap".into(), None));
        // explicit host / off pass through untouched
        assert_eq!(resolve_local_net("host", true, false), ("host".into(), None));
        assert_eq!(resolve_local_net("off", true, true), ("off".into(), None));
    }

    #[test]
    fn url_filename_strips_path_and_query() {
        assert_eq!(url_filename("https://h/a/b/model.gguf?download=true")
                       .as_deref(), Some("model.gguf"));
        assert_eq!(url_filename("https://h/"), None);
    }
}
