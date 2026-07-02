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
const DEFAULT_MODEL_URL: &str = "https://huggingface.co/Qwen/Qwen3-0.6B-GGUF/\
                                 resolve/main/Qwen3-0.6B-Q8_0.gguf";

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
  --dir DIR        where model+runtime live (default {state_home}/oaita/local)
  --model-url URL  GGUF to fetch (default Qwen3-0.6B-Q8_0 from Hugging Face)
  --runtime-url URL  llama.cpp release zip (default: latest CPU ubuntu-x64)
  --setup-only     download + write config, don't start the server
  --write-config   overwrite an existing oaita.toml (backed up to .bak)
  --force          re-download even if files exist
  --no-box         run on the host directly instead of inside a sarun box

By default the download + server run INSIDE a sarun box named
'oaita-local' (host-shared network, so the endpoint is 127.0.0.1 as
usual): the model/runtime writes are captured like any box's — review
them in the UI, apply to keep them on the host, discard to drop the
whole thing. Requires a running engine; --no-box skips the box.";

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
            // Internal: set by the box re-exec below — we ARE the in-box
            // payload; do the work directly.
            "--inbox" => inbox = true,
            "-h" | "--help" => { println!("{USAGE}"); return 0; }
            other => { eprintln!("oaita local: unknown flag {other:?}\n{USAGE}");
                       return 2; }
        }
    }
    let dir = dir.unwrap_or_else(|| crate::paths::oaita_state_home().join("local"));
    // Default: do the download + serve INSIDE a sarun box so the writes are
    // captured (review → apply to keep, discard to drop). The config file is
    // written host-side first — it's the pointer the host's oaita needs
    // either way. Host-shared network so the endpoint is 127.0.0.1 as usual.
    if !inbox && !no_box {
        if !engine_running() {
            eprintln!("oaita local: no running engine (needed to run the \
                       download in a box) — start `sarun` or `sarun serve`, \
                       or pass --no-box to download onto the host directly");
            return 1;
        }
        if let Err(e) = ensure_config(port, write_config) {
            eprintln!("oaita local: {e:#}");
            return 1;
        }
        let this = std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "sarun".into());
        let mut cmd = std::process::Command::new(&this);
        cmd.args(["run", "--net", "host", "oaita-local", "--",
                  &this, "oaita", "local", "--inbox",
                  "--port", &port.to_string(),
                  "--model-url", &model_url,
                  "--dir"]).arg(&dir);
        if let Some(u) = &runtime_url { cmd.args(["--runtime-url", u]); }
        if setup_only { cmd.arg("--setup-only"); }
        if force { cmd.arg("--force"); }
        eprintln!("oaita local: running in box 'oaita-local' — writes are \
                   captured; apply the box to keep the model/runtime, \
                   discard to drop them");
        return match cmd.status() {
            Ok(st) => st.code().unwrap_or(1),
            Err(e) => { eprintln!("oaita local: spawn box: {e}"); 1 }
        };
    }
    match run(&dir, port, &model_url, runtime_url.as_deref(),
              setup_only, write_config && !inbox, force, inbox) {
        Ok(()) => 0,
        Err(e) => { eprintln!("oaita local: {e:#}"); 1 }
    }
}

/// Whether the engine's control socket answers (a box run needs it).
fn engine_running() -> bool {
    std::os::unix::net::UnixStream::connect(crate::paths::sock_path()).is_ok()
}

fn run(dir: &Path, port: u16, model_url: &str, runtime_url: Option<&str>,
       setup_only: bool, write_config: bool, force: bool, inbox: bool)
       -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("mkdir {dir:?}"))?;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()
        .context("tokio runtime")?;

    // 1. model
    let model_name = url_filename(model_url)
        .ok_or_else(|| anyhow!("cannot derive a filename from {model_url}"))?;
    let model_path = dir.join(&model_name);
    if force || !model_path.is_file() {
        rt.block_on(fetch_to(model_url, &model_path, "model"))?;
    } else {
        eprintln!("model already present: {}", model_path.display());
    }

    // 2. runtime
    let server = dir.join("llama-server");
    if force || !server.is_file() {
        let url = match runtime_url {
            Some(u) => u.to_string(),
            None => rt.block_on(latest_runtime_url())?,
        };
        // Keep the URL's own extension: extract_runtime routes zip vs tar.gz
        // by filename.
        let arch_name = url_filename(&url).unwrap_or_else(|| "runtime.tar.gz".into());
        let arch_path = dir.join(&arch_name);
        rt.block_on(fetch_to(&url, &arch_path, "runtime"))?;
        let n = extract_runtime(&arch_path, dir)?;
        let _ = std::fs::remove_file(&arch_path);
        if !server.is_file() {
            bail!("archive extracted ({n} files) but no llama-server in it — \
                   pass --runtime-url with a llama.cpp *bin-ubuntu-x64* \
                   archive, or drop a llama-server binary into {}",
                  dir.display());
        }
        eprintln!("runtime ready: {} ({n} files)", server.display());
    } else {
        eprintln!("runtime already present: {}", server.display());
    }

    // 3. config — host-side concern; the box wrapper already wrote it (a
    //    second write here would only show up as a spurious box change).
    if !inbox {
        ensure_config(port, write_config)?;
    }

    if setup_only {
        eprintln!("setup complete — start the server later with `oaita local`");
        return Ok(());
    }

    // 4. serve (foreground; Ctrl-C stops it)
    serve(dir, &model_path, port)
}

/// GET `url` streaming to `dest` (via .part + rename), with coarse progress
/// on stderr. Follows redirects (Hugging Face resolves to a CDN).
async fn fetch_to(url: &str, dest: &Path, label: &str) -> Result<()> {
    eprintln!("downloading {label}: {url}");
    let client = reqwest::Client::builder()
        .user_agent("sarun-oaita-local")
        .build().context("http client")?;
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
    let client = reqwest::Client::builder()
        .user_agent("sarun-oaita-local")
        .build().context("http client")?;
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

/// Flatten the useful payload of a llama.cpp release archive (server binary
/// + its shared libs live under build/bin/) into `dir`. Handles both the
/// current .tar.gz assets and the older .zip ones. Returns files written.
fn extract_runtime(archive: &Path, dir: &Path) -> Result<usize> {
    let is_zip = archive.extension().is_some_and(|e| e == "zip");
    let mut n = 0;
    if is_zip {
        let f = std::fs::File::open(archive).context("open runtime archive")?;
        let mut z = zip::ZipArchive::new(f).context("read runtime zip")?;
        for i in 0..z.len() {
            let mut e = z.by_index(i).context("zip entry")?;
            if e.is_dir() { continue; }
            let name = Path::new(e.name()).file_name()
                .and_then(|s| s.to_str()).unwrap_or("").to_string();
            if !keep_runtime_file(&name) { continue; }
            let mut buf = Vec::new();
            e.read_to_end(&mut buf).context("read zip entry")?;
            install_runtime_file(dir, &name, &buf)?;
            n += 1;
        }
    } else {
        let f = std::fs::File::open(archive).context("open runtime archive")?;
        let gz = flate2::read::GzDecoder::new(f);
        let mut t = tar::Archive::new(gz);
        for e in t.entries().context("read runtime tar")? {
            let mut e = e.context("tar entry")?;
            if !e.header().entry_type().is_file() { continue; }
            let name = e.path().ok()
                .and_then(|p| p.file_name()
                    .and_then(|s| s.to_str()).map(str::to_string))
                .unwrap_or_default();
            if !keep_runtime_file(&name) { continue; }
            let mut buf = Vec::new();
            e.read_to_end(&mut buf).context("read tar entry")?;
            install_runtime_file(dir, &name, &buf)?;
            n += 1;
        }
    }
    Ok(n)
}

/// The oaita.toml contents pointing at the local server.
fn local_config(port: u16) -> String {
    format!("# written by `oaita local` — a llama.cpp server on this machine.\n\
             model = \"local\"\n\
             base_url = \"http://127.0.0.1:{port}/v1\"\n\
             api_key = \"sk-local\"\n")
}

/// Write oaita.toml for the local endpoint. An existing config is left
/// alone unless --write-config (then backed up to oaita.toml.bak first).
fn ensure_config(port: u16, overwrite: bool) -> Result<()> {
    let p = crate::paths::oaita_config_path();
    if p.is_file() && !overwrite {
        eprintln!("keeping existing {} (use --write-config to point it at \
                   the local server)", p.display());
        return Ok(());
    }
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {parent:?}"))?;
    }
    if p.is_file() {
        let bak = p.with_extension("toml.bak");
        std::fs::copy(&p, &bak).with_context(|| format!("backup to {bak:?}"))?;
        eprintln!("backed up existing config to {}", bak.display());
    }
    std::fs::write(&p, local_config(port)).with_context(|| format!("write {p:?}"))?;
    eprintln!("wrote {}", p.display());
    Ok(())
}

/// Run llama-server in the foreground on `port`, announce readiness once
/// /health answers, and wait until it exits (Ctrl-C).
fn serve(dir: &Path, model: &Path, port: u16) -> Result<()> {
    let server = dir.join("llama-server");
    eprintln!("starting {} on 127.0.0.1:{port} …", server.display());
    let mut child = std::process::Command::new(&server)
        .arg("-m").arg(model)
        .args(["--host", "127.0.0.1", "--port", &port.to_string(),
               "--jinja", "-c", "8192"])
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
            eprintln!();
            eprintln!("ready — local OpenAI-compatible endpoint: \
                       http://127.0.0.1:{port}/v1");
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
                ("build/bin/libggml-base.so", b"ELF".as_slice()),
                ("build/bin/llama-cli", b"ELF".as_slice()),
                ("README.md", b"docs".as_slice()),
            ] {
                let mut h = tar::Header::new_gnu();
                h.set_size(body.len() as u64);
                h.set_mode(0o755);
                h.set_cksum();
                t.append_data(&mut h, name, body).unwrap();
            }
            t.into_inner().unwrap().finish().unwrap().flush().unwrap();
        }
        let out = tmp.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let n = extract_runtime(&tp, &out).unwrap();
        assert_eq!(n, 2, "server + lib; no cli/docs");
        assert!(out.join("llama-server").is_file());
        assert!(out.join("libggml-base.so").is_file());
        assert!(!out.join("llama-cli").exists());
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
        let c = local_config(18181);
        assert!(c.contains("base_url = \"http://127.0.0.1:18181/v1\""));
        assert!(c.contains("model = \"local\""));
        // parses as the Config the rest of oaita reads
        let v: toml::Value = c.parse().unwrap();
        assert_eq!(v.get("api_key").and_then(|x| x.as_str()), Some("sk-local"));
    }

    #[test]
    fn url_filename_strips_path_and_query() {
        assert_eq!(url_filename("https://h/a/b/model.gguf?download=true")
                       .as_deref(), Some("model.gguf"));
        assert_eq!(url_filename("https://h/"), None);
    }
}
