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
            // the chromedp/headless-shell image (the cellulose box image)
            "/headless-shell/headless-shell",
            "/opt/pw-browsers/chromium-1194/chrome-linux/chrome",
            "chromium",
            "chromium-browser",
            "google-chrome",
            "headless_shell",
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

/// Spawn headless Chromium wired to a CDP pipe on fds 3/4. `spki`, when set,
/// is the sarun MITM root's SubjectPublicKeyInfo hash — passed as
/// `--ignore-certificate-errors-spki-list` so a Chromium in a MITM'd tap box
/// trusts the engine's leaf certs (Chromium reads neither the overlay CA
/// bundle nor SSL_CERT_FILE).
pub fn spawn_host_chromium(spki: Option<&str>) -> Result<HostBrowser> {
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
    if let Some(k) = spki {
        cmd.arg(format!("--ignore-certificate-errors-spki-list={k}"));
    }
    // Honor an ambient proxy (this is a standalone host browser, not a tap
    // box). On a direct connection, or inside a tap box (transparent MITM),
    // Chromium needs nothing.
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

/// Render one grid row to a 24-bit ANSI string (self-contained: resets at end).
fn row_to_ansi(row: &[crate::browser::render::Cell]) -> String {
    let mut out = String::new();
    let (mut lf, mut lb): (Option<Rgb>, Option<Rgb>) = (None, None);
    for cell in row {
        if cell.ch.is_empty() {
            continue;
        }
        if Some(cell.fg) != lf {
            out.push_str(&format!("\x1b[38;2;{};{};{}m", cell.fg.0, cell.fg.1, cell.fg.2));
            lf = Some(cell.fg);
        }
        if Some(cell.bg) != lb {
            out.push_str(&format!("\x1b[48;2;{};{};{}m", cell.bg.0, cell.bg.1, cell.bg.2));
            lb = Some(cell.bg);
        }
        out.push_str(&cell.ch);
    }
    out.push_str("\x1b[0m");
    out
}

// ── interactive TUI (DESIGN-cellulose.md E2) ────────────────────────────────
//
// The port of the prototype's interactive() loop: raw mode, screencast-driven
// refresh, per-row diff redraw, and input mapped to CDP. This is what the box
// runs; the sarun UI embeds it as a PTY pane exactly like carbonyl.

/// Current terminal size in cells, or a default.
fn term_size() -> (usize, usize) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0 {
        (ws.ws_col as usize, ws.ws_row as usize)
    } else {
        (100, 36)
    }
}

fn set_raw(fd: i32) -> Option<libc::termios> {
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut t) } != 0 {
        return None;
    }
    let saved = t;
    unsafe {
        libc::cfmakeraw(&mut t);
        libc::tcsetattr(fd, libc::TCSANOW, &t);
    }
    Some(saved)
}

fn restore_raw(fd: i32, saved: &libc::termios) {
    unsafe { libc::tcsetattr(fd, libc::TCSADRAIN, saved) };
}

/// Poll fd for readability with a millisecond timeout.
fn poll_in(fd: i32, ms: i32) -> bool {
    let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    unsafe { libc::poll(&mut pfd, 1, ms) > 0 && (pfd.revents & libc::POLLIN) != 0 }
}

/// Split one input token off the front of `buf`; returns (token, consumed).
/// (None, 0) means an incomplete escape sequence — wait for more bytes.
fn next_token(buf: &[u8]) -> (Option<Vec<u8>>, usize) {
    if buf.is_empty() {
        return (None, 0);
    }
    if buf[0] == 0x1b {
        if buf.len() == 1 {
            return (Some(vec![0x1b]), 1); // lone ESC
        }
        if buf[1] == b'[' {
            // CSI: ESC [ params final(@-~)
            let mut i = 2;
            while i < buf.len() && !(0x40..=0x7e).contains(&buf[i]) {
                i += 1;
            }
            if i < buf.len() {
                return (Some(buf[..=i].to_vec()), i + 1);
            }
            if buf.len() < 32 {
                return (None, 0); // still arriving
            }
            return (Some(vec![0x1b]), 1); // garbage; drop ESC
        }
        return (Some(buf[..2].to_vec()), 2); // alt-modified key
    }
    if buf[0] < 0x20 || buf[0] == 0x7f {
        return (Some(vec![buf[0]]), 1);
    }
    let mut n = 1;
    while n < buf.len() && buf[n] >= 0x20 && buf[n] != 0x7f {
        n += 1;
    }
    (Some(buf[..n].to_vec()), n)
}

/// Draw a frame with per-row diffing; returns this frame's rows for the next
/// diff. `frame_png` is the latest screencast frame (or None → pull a frame).
fn draw(
    sess: &BrowserSession,
    out: &mut impl Write,
    url_edit: Option<&str>,
    frame_png: Option<&[u8]>,
    prev: Option<&Vec<String>>,
) -> Result<Vec<String>> {
    let grid = match frame_png {
        Some(png) => sess.frame_from_png(png)?,
        None => sess.frame()?,
    };
    let (url, title) = sess.url_and_title();
    let status = match url_edit {
        Some(e) => format!(" url: {e}_"),
        None => format!(" {title}  {url}  [^L url ^R reload alt-←→ hist ^Q quit]"),
    };
    let status: String = status.chars().map(|c| if c < ' ' { ' ' } else { c }).collect();
    let status: String = status.chars().take(sess.cols).collect();
    let status = format!("{status:<width$}", width = sess.cols);

    let mut lines: Vec<String> = grid.iter().map(|r| row_to_ansi(r)).collect();
    lines.push(format!("\x1b[7m{status}\x1b[0m"));
    for (i, line) in lines.iter().enumerate() {
        if let Some(p) = prev {
            if p.get(i) == Some(line) {
                continue;
            }
        }
        write!(out, "\x1b[{};1H{}", i + 1, line)?;
    }
    out.flush()?;
    Ok(lines)
}

/// Run the interactive browser TUI against an attached session.
pub fn interactive(sess: &BrowserSession) -> Result<()> {
    use base64::Engine as _;
    let mut out = std::io::stdout();
    let saved = set_raw(0);
    write!(out, "\x1b[?1049h\x1b[?25l\x1b[?1002h\x1b[?1006h")?;
    out.flush()?;
    sess.start_screencast()?;

    let mut prev: Option<Vec<String>> = None;
    let mut frame_png: Option<Vec<u8>> = None;
    let mut inbuf: Vec<u8> = Vec::new();
    let mut url_edit: Option<String> = None;
    let mut dirty = true;

    let result = (|| -> Result<()> {
        loop {
            if dirty {
                prev = Some(draw(sess, &mut out, url_edit.as_deref(),
                                 frame_png.as_deref(), prev.as_ref())?);
                dirty = false;
            }
            let ready = poll_in(0, 100);
            for ev in sess.cdp().drain_events() {
                match ev.get("method").and_then(|m| m.as_str()) {
                    Some("Page.screencastFrame") => {
                        let sid = ev["params"]["sessionId"].as_i64().unwrap_or(0);
                        sess.ack_frame(sid);
                        if let Some(d) = ev["params"]["data"].as_str() {
                            if let Ok(png) = base64::engine::general_purpose::STANDARD.decode(d) {
                                frame_png = Some(png);
                                dirty = true;
                            }
                        }
                    }
                    Some("Page.loadEventFired") | Some("Page.frameNavigated") => {
                        let _ = sess.start_screencast(); // navigation stops the cast
                        dirty = true;
                    }
                    _ => {}
                }
            }
            if sess.cdp().is_closed() {
                return Ok(());
            }
            if !ready {
                continue;
            }
            let mut tmp = [0u8; 4096];
            let n = unsafe { libc::read(0, tmp.as_mut_ptr() as *mut _, tmp.len()) };
            if n <= 0 {
                return Ok(());
            }
            inbuf.extend_from_slice(&tmp[..n as usize]);
            dirty = true;
            loop {
                let (tok, used) = next_token(&inbuf);
                let Some(tok) = tok else { break };
                inbuf.drain(..used);
                if let Some(edit) = url_edit.as_mut() {
                    match tok.as_slice() {
                        b"\r" => {
                            if !edit.is_empty() {
                                let u = normalize_url(edit);
                                let _ = sess.navigate(&u);
                            }
                            url_edit = None;
                        }
                        b"\x1b" | b"\x11" => url_edit = None,
                        b"\x7f" => {
                            edit.pop();
                        }
                        t if t[0] >= 0x20 => edit.push_str(&String::from_utf8_lossy(t)),
                        _ => {}
                    }
                    continue;
                }
                match tok.as_slice() {
                    b"\x11" => return Ok(()),                 // Ctrl-Q
                    b"\x0c" => url_edit = Some(String::new()), // Ctrl-L
                    b"\x12" => { let _ = sess.reload(); }      // Ctrl-R
                    b"\x02" | b"\x1b[1;3D" | b"\x1b\x1b[D" => { let _ = sess.history_go(-1); }
                    b"\x06" | b"\x1b[1;3C" | b"\x1b\x1b[C" => { let _ = sess.history_go(1); }
                    b"\x1b[A" => { let _ = sess.scroll(-3); }
                    b"\x1b[B" => { let _ = sess.scroll(3); }
                    b"\x1b[5~" => { let _ = sess.scroll(-(sess.rows as i64 - 2)); }
                    b"\x1b[6~" => { let _ = sess.scroll(sess.rows as i64 - 2); }
                    b"\r" => { let _ = sess.key("Enter", "Enter", 13, "\r"); }
                    b"\t" => { let _ = sess.key("Tab", "Tab", 9, ""); }
                    b"\x7f" => { let _ = sess.key("Backspace", "Backspace", 8, ""); }
                    t if t.starts_with(b"\x1b[<") => {
                        if let Some((col, row, press)) = parse_sgr_mouse(t) {
                            if press {
                                let _ = sess.click(col, row);
                            }
                        }
                    }
                    t if !t.starts_with(b"\x1b") => {
                        let _ = sess.type_text(&String::from_utf8_lossy(t));
                    }
                    _ => {}
                }
            }
        }
    })();

    write!(out, "\x1b[?1006l\x1b[?1002l\x1b[?25h\x1b[?1049l")?;
    out.flush()?;
    if let Some(s) = saved {
        restore_raw(0, &s);
    }
    result
}

/// Parse an SGR mouse report `ESC [ < b ; col ; row (M|m)` → (col0, row0,
/// is_left_press).
fn parse_sgr_mouse(t: &[u8]) -> Option<(usize, usize, bool)> {
    let s = std::str::from_utf8(t).ok()?;
    let body = s.strip_prefix("\x1b[<")?;
    let press = body.ends_with('M');
    let body = &body[..body.len() - 1];
    let mut it = body.split(';');
    let b: i64 = it.next()?.parse().ok()?;
    let col: usize = it.next()?.parse().ok()?;
    let row: usize = it.next()?.parse().ok()?;
    Some((col.saturating_sub(1), row.saturating_sub(1), press && b == 0))
}

fn normalize_url(u: &str) -> String {
    if u.contains("://") || u.starts_with("data:") || u.starts_with("about:") {
        u.to_string()
    } else {
        format!("https://{u}")
    }
}

// ── the `sarun browser` CLI verb ────────────────────────────────────────────

const HELP: &str = "\
sarun browser — a web browser in the terminal (engine-native, CDP)

usage:
    sarun browser [--dump|--dump-text] [--size WxH] URL

    (no flag)     interactive: full-screen TUI (needs a tty)
    --dump        one 24-bit-color ANSI frame, then exit
    --dump-text   one plain-text frame (no pixels), then exit
    --size WxH    grid size in cells (default: terminal size)
    --spki KEY    trust a MITM root by SPKI hash (for tap boxes)

Interactive keys: ^Q quit, ^L url bar, ^R reload, alt-←/→ back/forward,
arrows/PgUp/PgDn scroll, mouse click, typing goes to the page.

Drives a stock headless Chromium over the DevTools Protocol; no carbonyl.
Set $CELLULOSE_BROWSER to choose the browser binary.";

/// Entry point for `sarun browser …`. Returns a process exit code.
pub fn browser_cli(args: &[String]) -> i32 {
    let mut mode = "interactive";
    let mut size: Option<(usize, usize)> = None;
    let mut url: Option<String> = None;
    let mut spki: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                println!("{HELP}");
                return 0;
            }
            "--dump" => mode = "dump",
            "--dump-text" => mode = "text",
            "--spki" => spki = it.next().cloned(),
            "--size" => match it.next().and_then(|s| parse_size(s)) {
                Some(sz) => size = Some(sz),
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
    // Interactive needs a tty on both ends; otherwise degrade to a dump.
    let is_tty = unsafe { libc::isatty(0) == 1 && libc::isatty(1) == 1 };
    if mode == "interactive" && !is_tty {
        mode = "dump";
    }
    let result = if mode == "interactive" {
        let (cols, trows) = size.unwrap_or_else(term_size);
        run_interactive(&url, cols, trows.saturating_sub(1).max(1), spki.as_deref())
    } else {
        let (cols, rows) = size.unwrap_or((100, 36));
        render_once(&url, cols, rows, mode, spki.as_deref())
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("browser: {e:#}");
            1
        }
    }
}

/// Spawn Chromium, attach a session sized to the terminal, navigate, and run
/// the interactive TUI. `rows` is the content height (the status bar adds one).
fn run_interactive(url: &str, cols: usize, rows: usize, spki: Option<&str>) -> Result<i32> {
    let url = normalize_url(url);
    let browser = spawn_host_chromium(spki)?;
    let sess = BrowserSession::attach(browser.cdp.clone(), cols, rows)?;
    sess.navigate(&url)?;
    sess.wait_load(Duration::from_secs(15));
    std::thread::sleep(Duration::from_millis(300)); // font settle
    interactive(&sess)?;
    Ok(0)
}

fn parse_size(s: &str) -> Option<(usize, usize)> {
    let (c, r) = s.split_once('x')?;
    Some((c.parse().ok()?, r.parse().ok()?))
}

fn render_once(url: &str, cols: usize, rows: usize, mode: &str, spki: Option<&str>) -> Result<i32> {
    let url = if url.contains("://") || url.starts_with("data:") || url.starts_with("about:") {
        url.to_string()
    } else {
        format!("https://{url}")
    };
    let browser = spawn_host_chromium(spki)?;
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
