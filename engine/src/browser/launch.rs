// Launching Chromium for cellulose and the `sarun browser` CLI verb
// (DESIGN-cellulose.md C5-C/D).
//
// The host launcher spawns headless Chromium directly and speaks CDP over
// `--remote-debugging-pipe` (fds 3/4 wired via pre_exec, the same fd-passing
// idiom the box path uses). The CLI verb renders one page to the terminal —
// the engine-native equivalent of the prototype's `--dump` / `--dump-text`.

use std::io::Write;
use std::os::fd::RawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use super::cdp::{pipe_transport, Cdp};
use super::render::{Grid, Rgb};
use super::session::{grid_to_text, BrowserSession};

/// Candidate Chromium binaries, in order. `$CELLULOSE_BROWSER` wins.
fn find_browser() -> Result<String> {
    let mut cands: Vec<String> = Vec::new();
    if let Ok(b) = std::env::var("CELLULOSE_BROWSER") {
        cands.push(b);
    }
    cands.extend(
        [
            "/opt/pw-browsers/chromium-1194/chrome-linux/chrome",
            "chromium",
            "chromium-browser",
            "google-chrome",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    for c in &cands {
        if c.contains('/') {
            if std::path::Path::new(c).exists() {
                return Ok(c.clone());
            }
        } else if let Ok(p) = which(c) {
            return Ok(p);
        }
    }
    anyhow::bail!("no Chromium found; set $CELLULOSE_BROWSER")
}

fn which(name: &str) -> Result<String> {
    let path = std::env::var("PATH").unwrap_or_default();
    for dir in path.split(':') {
        let p = std::path::Path::new(dir).join(name);
        if p.exists() {
            return Ok(p.to_string_lossy().into_owned());
        }
    }
    anyhow::bail!("{name} not on PATH")
}

/// A live headless Chromium plus its CDP client. Dropping it kills the child.
pub struct HostBrowser {
    child: Child,
    pub cdp: Arc<Cdp>,
}

impl Drop for HostBrowser {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn headless Chromium wired to a CDP pipe on fds 3/4.
pub fn spawn_host_chromium() -> Result<HostBrowser> {
    // engine_read ← chromium fd 4 (child_w); engine_write → chromium fd 3 (child_r)
    let (engine_read, child_w) = os_pipe()?;
    let (child_r, engine_write) = os_pipe()?;
    let bin = find_browser()?;
    let (cr, cw) = (child_r, child_w);
    let mut cmd = Command::new(bin);
    cmd.args([
        "--headless",
        "--no-sandbox",
        "--disable-gpu",
        "--remote-debugging-pipe",
        "--disable-features=EncryptedClientHello",
        "--force-color-profile=srgb",
        "--disable-dev-shm-usage",
        "--hide-scrollbars",
        "--mute-audio",
        "about:blank",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    // Honor an ambient proxy (this is a standalone host browser, not a tap
    // box). On a direct connection Chromium needs nothing.
    if let Ok(proxy) = std::env::var("HTTPS_PROXY").or_else(|_| std::env::var("https_proxy")) {
        cmd.arg(format!("--proxy-server={proxy}"));
    }
    // Wire the child ends to fds 3 (read) and 4 (write). Rust's Command does
    // not close arbitrary inherited fds, so the dup'd 3/4 survive to exec.
    unsafe {
        cmd.pre_exec(move || {
            if libc::dup2(cr, 3) < 0 || libc::dup2(cw, 4) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn().context("spawn chromium")?;
    // Parent no longer needs the child ends.
    unsafe {
        libc::close(child_r);
        libc::close(child_w);
    }
    let (r, w) = pipe_transport(engine_read, engine_write);
    Ok(HostBrowser { child, cdp: Cdp::new(r, w) })
}

/// A raw pipe pair `(read_fd, write_fd)`.
fn os_pipe() -> Result<(RawFd, RawFd)> {
    let mut fds = [0 as libc::c_int; 2];
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if rc != 0 {
        anyhow::bail!("pipe: {}", std::io::Error::last_os_error());
    }
    Ok((fds[0], fds[1]))
}

// ── ANSI rendering ──────────────────────────────────────────────────────────

/// Render a grid to a 24-bit-color ANSI frame (one line per row).
pub fn grid_to_ansi(grid: &Grid) -> String {
    let mut out = String::new();
    for row in grid {
        let (mut last_fg, mut last_bg): (Option<Rgb>, Option<Rgb>) = (None, None);
        for cell in row {
            if cell.ch.is_empty() {
                continue; // wide continuation
            }
            if Some(cell.fg) != last_fg {
                out.push_str(&format!("\x1b[38;2;{};{};{}m", cell.fg.0, cell.fg.1, cell.fg.2));
                last_fg = Some(cell.fg);
            }
            if Some(cell.bg) != last_bg {
                out.push_str(&format!("\x1b[48;2;{};{};{}m", cell.bg.0, cell.bg.1, cell.bg.2));
                last_bg = Some(cell.bg);
            }
            out.push_str(&cell.ch);
        }
        out.push_str("\x1b[0m\n");
    }
    out
}

// ── the `sarun browser` CLI verb ────────────────────────────────────────────

const HELP: &str = "\
sarun browser — render a web page in the terminal (engine-native, CDP)

usage:
    sarun browser [--dump|--dump-text] [--size WxH] URL

    --dump        one 24-bit-color ANSI frame (default)
    --dump-text   one plain-text frame (no pixels)
    --size WxH    grid size in cells (default 100x36)

Drives a stock headless Chromium over the DevTools Protocol; no carbonyl.
Set $CELLULOSE_BROWSER to choose the browser binary.";

/// Entry point for `sarun browser …`. Returns a process exit code.
pub fn browser_cli(args: &[String]) -> i32 {
    let mut mode = "dump";
    let (mut cols, mut rows) = (100usize, 36usize);
    let mut url: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                println!("{HELP}");
                return 0;
            }
            "--dump" => mode = "dump",
            "--dump-text" => mode = "text",
            "--size" => match it.next().and_then(|s| parse_size(s)) {
                Some((c, r)) => {
                    cols = c;
                    rows = r;
                }
                None => {
                    eprintln!("browser: --size wants WxH, e.g. 100x36");
                    return 2;
                }
            },
            s if s.starts_with('-') => {
                eprintln!("browser: unknown flag {s}");
                return 2;
            }
            s => url = Some(s.to_string()),
        }
    }
    let Some(url) = url else {
        eprintln!("{HELP}");
        return 2;
    };
    match render_once(&url, cols, rows, mode) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("browser: {e:#}");
            1
        }
    }
}

fn parse_size(s: &str) -> Option<(usize, usize)> {
    let (c, r) = s.split_once('x')?;
    Some((c.parse().ok()?, r.parse().ok()?))
}

fn render_once(url: &str, cols: usize, rows: usize, mode: &str) -> Result<i32> {
    let url = if url.contains("://") || url.starts_with("data:") || url.starts_with("about:") {
        url.to_string()
    } else {
        format!("https://{url}")
    };
    let browser = spawn_host_chromium()?;
    let sess = BrowserSession::attach(browser.cdp.clone(), cols, rows)?;
    let mut code = 0;
    if let Some(err) = sess.navigate(&url)? {
        eprintln!("browser: navigation failed: {err}");
        code = 2;
    }
    sess.wait_load(Duration::from_secs(15));
    std::thread::sleep(Duration::from_millis(400)); // font settle
    let grid = sess.frame()?;
    let out = if mode == "text" {
        grid_to_text(&grid)
    } else {
        grid_to_ansi(&grid)
    };
    // Tolerate a closed pipe (`| head`).
    let _ = std::io::stdout().write_all(out.as_bytes());
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::render::Cell;

    #[test]
    fn ansi_has_truecolor_escapes() {
        let grid = vec![vec![Cell {
            ch: "A".into(),
            fg: (255, 0, 0),
            bg: (0, 0, 0),
        }]];
        let s = grid_to_ansi(&grid);
        assert!(s.contains("\x1b[38;2;255;0;0m"));
        assert!(s.contains("\x1b[48;2;0;0;0m"));
        assert!(s.contains('A'));
    }

    #[test]
    fn size_parsing() {
        assert_eq!(parse_size("100x36"), Some((100, 36)));
        assert_eq!(parse_size("bad"), None);
    }

}
