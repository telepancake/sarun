// A browser session: a CDP connection plus the render pipeline that turns a
// live page into a cell grid (DESIGN-cellulose.md C2/C5-C).
//
// This increment wires the full pipeline against a Chromium the caller has
// already launched and reached over any `Cdp` transport. The host-spawn helper
// here is for the CLI/`--dump` path and the integration test; increment C's
// box bridge will provide the same `Cdp` from a Chromium inside a tap box, and
// nothing else in this file changes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::cdp::Cdp;
use super::font::{CELL_H, CELL_W, inject_js};
use super::render::{self, Grid, Rgb};

const CALL: Duration = Duration::from_secs(20);

/// One browser tab: its CDP target plus the attached flat session.
struct Tab {
    target_id: String,
    session_id: String,
}

pub struct BrowserSession {
    cdp: Arc<Cdp>,
    tabs: Vec<Tab>,
    active: usize,
    pub cols: usize,
    pub rows: usize,
}

impl BrowserSession {
    /// Attach to the first page target on `cdp`, apply the forced font +
    /// device metrics, and return a session ready to `navigate`.
    pub fn attach(cdp: Arc<Cdp>, cols: usize, rows: usize) -> Result<Self> {
        let targets = cdp.call("Target.getTargets", json!({}), None, CALL)?;
        let page = targets["targetInfos"]
            .as_array()
            .and_then(|ts| ts.iter().find(|t| t["type"] == "page"))
            .context("no page target")?;
        let target_id = page["targetId"]
            .as_str()
            .context("no targetId")?
            .to_string();
        let attached = cdp.call(
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
            None,
            CALL,
        )?;
        let session_id = attached["sessionId"]
            .as_str()
            .context("attach: no sessionId")?
            .to_string();

        let me = Self {
            cdp,
            tabs: vec![Tab {
                target_id,
                session_id: session_id.clone(),
            }],
            active: 0,
            cols,
            rows,
        };
        me.setup_target(&session_id)?;
        Ok(me)
    }

    /// Per-tab CDP setup: enable the domains, force device metrics, inject the
    /// cell font on every document. Run once per target (tab).
    fn setup_target(&self, sid: &str) -> Result<()> {
        let s = Some(sid);
        self.cdp.call("Page.enable", json!({}), s, CALL)?;
        self.cdp.call("DOM.enable", json!({}), s, CALL)?;
        self.cdp.call("DOMSnapshot.enable", json!({}), s, CALL)?;
        self.cdp.call(
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": self.cols as i64 * CELL_W,
                "height": self.rows as i64 * CELL_H,
                "deviceScaleFactor": 1,
                "mobile": false
            }),
            s,
            CALL,
        )?;
        self.cdp.call(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": inject_js(), "runImmediately": true }),
            s,
            CALL,
        )?;
        Ok(())
    }

    fn s(&self) -> Option<&str> {
        Some(self.tabs[self.active].session_id.as_str())
    }

    /// The active tab's flat session id (for filtering its screencast frames).
    pub fn active_session(&self) -> &str {
        &self.tabs[self.active].session_id
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    /// Open a new tab (a fresh page target), set it active, and navigate it.
    /// The caller re-issues start_screencast on the new active session.
    pub fn new_tab(&mut self, url: &str) -> Result<()> {
        let created = self.cdp.call(
            "Target.createTarget",
            json!({ "url": "about:blank" }),
            None,
            CALL,
        )?;
        let target_id = created["targetId"]
            .as_str()
            .context("createTarget: no id")?
            .to_string();
        let attached = self.cdp.call(
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
            None,
            CALL,
        )?;
        let session_id = attached["sessionId"]
            .as_str()
            .context("no sessionId")?
            .to_string();
        self.setup_target(&session_id)?;
        self.tabs.push(Tab {
            target_id,
            session_id,
        });
        self.active = self.tabs.len() - 1;
        self.navigate(url)?;
        Ok(())
    }

    /// Close the active tab. Returns false if it was the last one (caller
    /// should quit). Otherwise the previous tab becomes active.
    pub fn close_tab(&mut self) -> bool {
        if self.tabs.len() <= 1 {
            return false;
        }
        let tab = self.tabs.remove(self.active);
        let _ = self.cdp.call(
            "Target.closeTarget",
            json!({ "targetId": tab.target_id }),
            None,
            CALL,
        );
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        }
        true
    }

    /// Switch to the next / previous tab (wraps).
    pub fn cycle_tab(&mut self, delta: isize) {
        let n = self.tabs.len();
        if n > 1 {
            self.active = (self.active as isize + delta).rem_euclid(n as isize) as usize;
        }
    }

    /// Navigate; returns any `errorText` reported by the browser.
    pub fn navigate(&self, url: &str) -> Result<Option<String>> {
        let res = self
            .cdp
            .call("Page.navigate", json!({ "url": url }), self.s(), CALL)?;
        Ok(res
            .get("errorText")
            .and_then(Value::as_str)
            .map(str::to_string))
    }

    /// Block until the page's load event fires (or the timeout elapses).
    pub fn wait_load(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            for ev in self.cdp.drain_events() {
                if ev.get("method").and_then(Value::as_str) == Some("Page.loadEventFired") {
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    /// Scroll by `dy_cells` terminal rows (positive = down).
    pub fn scroll(&self, dy_cells: i64) -> Result<()> {
        self.cdp.call(
            "Input.dispatchMouseEvent",
            json!({
                "type": "mouseWheel",
                "x": self.cols as i64 * CELL_W / 2,
                "y": self.rows as i64 * CELL_H / 2,
                "deltaX": 0,
                "deltaY": dy_cells * CELL_H
            }),
            self.s(),
            CALL,
        )?;
        Ok(())
    }

    /// Click at a terminal cell (col, row), centered in the cell.
    pub fn click(&self, col: usize, row: usize) -> Result<()> {
        let x = (col as i64 * CELL_W) + CELL_W / 2;
        let y = (row as i64 * CELL_H) + CELL_H / 2;
        for typ in ["mousePressed", "mouseReleased"] {
            self.cdp.call(
                "Input.dispatchMouseEvent",
                json!({ "type": typ, "x": x, "y": y, "button": "left", "clickCount": 1 }),
                self.s(),
                CALL,
            )?;
        }
        Ok(())
    }

    /// Type literal text into the focused element.
    pub fn type_text(&self, text: &str) -> Result<()> {
        self.cdp
            .call("Input.insertText", json!({ "text": text }), self.s(), CALL)?;
        Ok(())
    }

    /// Dispatch a named key (Enter/Tab/Backspace/arrows…).
    pub fn key(&self, key: &str, code: &str, vk: i64, text: &str) -> Result<()> {
        let mut down = json!({
            "type": "rawKeyDown", "key": key, "code": code,
            "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk
        });
        if !text.is_empty() {
            down["type"] = json!("keyDown");
            down["text"] = json!(text);
            down["unmodifiedText"] = json!(text);
        }
        self.cdp
            .call("Input.dispatchKeyEvent", down, self.s(), CALL)?;
        self.cdp.call(
            "Input.dispatchKeyEvent",
            json!({ "type": "keyUp", "key": key, "code": code,
                    "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk }),
            self.s(),
            CALL,
        )?;
        Ok(())
    }

    pub fn reload(&self) -> Result<()> {
        self.cdp.call("Page.reload", json!({}), self.s(), CALL)?;
        Ok(())
    }

    /// Walk the navigation history by `delta` (-1 back, +1 forward).
    pub fn history_go(&self, delta: i64) -> Result<()> {
        let hist = self
            .cdp
            .call("Page.getNavigationHistory", json!({}), self.s(), CALL)?;
        let cur = hist["currentIndex"].as_i64().unwrap_or(0);
        let entries = hist["entries"].as_array().cloned().unwrap_or_default();
        let i = cur + delta;
        if i >= 0 && (i as usize) < entries.len() {
            let id = entries[i as usize]["id"].clone();
            self.cdp.call(
                "Page.navigateToHistoryEntry",
                json!({ "entryId": id }),
                self.s(),
                CALL,
            )?;
        }
        Ok(())
    }

    /// Current URL and title from the navigation history.
    pub fn url_and_title(&self) -> (String, String) {
        if let Ok(h) = self
            .cdp
            .call("Page.getNavigationHistory", json!({}), self.s(), CALL)
        {
            let i = h["currentIndex"].as_u64().unwrap_or(0) as usize;
            if let Some(e) = h["entries"].as_array().and_then(|a| a.get(i)) {
                return (
                    e["url"].as_str().unwrap_or("").to_string(),
                    e["title"].as_str().unwrap_or("").to_string(),
                );
            }
        }
        (String::new(), String::new())
    }

    /// Start push-based frame delivery (Page.screencastFrame events fire only
    /// when the compositor produced a new frame).
    pub fn start_screencast(&self) -> Result<()> {
        self.cdp.call(
            "Page.startScreencast",
            json!({ "format": "png", "everyNthFrame": 1 }),
            self.s(),
            CALL,
        )?;
        Ok(())
    }

    /// Stop the active tab's screencast (before switching away from it, so a
    /// background tab never keeps pushing frames).
    pub fn stop_screencast(&self) {
        let _ = self
            .cdp
            .call("Page.stopScreencast", json!({}), self.s(), CALL);
    }

    pub fn ack_frame(&self, frame_session: i64) {
        let _ = self.cdp.call(
            "Page.screencastFrameAck",
            json!({ "sessionId": frame_session }),
            self.s(),
            CALL,
        );
    }

    /// Compose a grid from a screencast frame's PNG bytes plus a fresh DOM
    /// snapshot (text lands crisp on top of the downscaled pixels).
    pub fn frame_from_png(&self, png: &[u8]) -> Result<Grid> {
        let snap = self.snapshot()?;
        let text = render::snapshot_text(&snap, self.cols, self.rows);
        Ok(self.compose_png(png, &text))
    }

    fn compose_png(&self, png: &[u8], text: &[render::Placement]) -> Grid {
        let img = match image::load_from_memory(png) {
            Ok(i) => i.to_rgb8(),
            Err(_) => {
                return vec![
                    vec![
                        render::Cell {
                            ch: " ".into(),
                            fg: (0, 0, 0),
                            bg: (0, 0, 0),
                        };
                        self.cols
                    ];
                    self.rows
                ];
            }
        };
        let resized = image::imageops::resize(
            &img,
            (self.cols * 2) as u32,
            (self.rows * 2) as u32,
            image::imageops::FilterType::Triangle,
        );
        let sample = |x: usize, y: usize| -> Rgb {
            let p = resized.get_pixel(x as u32, y as u32);
            (p[0], p[1], p[2])
        };
        render::compose(sample, text, self.cols, self.rows)
    }

    /// Access the underlying CDP client (for event draining in the UI loop).
    pub fn cdp(&self) -> &Arc<Cdp> {
        &self.cdp
    }

    fn snapshot(&self) -> Result<Value> {
        self.cdp.call(
            "DOMSnapshot.captureSnapshot",
            json!({ "computedStyles": ["color", "visibility", "opacity"] }),
            self.s(),
            CALL,
        )
    }

    /// Raw PNG bytes of the current viewport.
    fn screenshot_png(&self) -> Result<Vec<u8>> {
        let res = self.cdp.call(
            "Page.captureScreenshot",
            json!({ "format": "png" }),
            self.s(),
            CALL,
        )?;
        let b64 = res["data"].as_str().context("screenshot: no data")?;
        use base64::Engine as _;
        Ok(base64::engine::general_purpose::STANDARD.decode(b64)?)
    }

    /// Render the current page into a cell grid: text from the DOM snapshot,
    /// pixels from the screenshot resized to two subpixels per cell each axis.
    pub fn frame(&self) -> Result<Grid> {
        let snap = self.snapshot()?;
        let text = render::snapshot_text(&snap, self.cols, self.rows);
        let png = self.screenshot_png()?;
        let img = image::load_from_memory(&png)
            .context("decode screenshot")?
            .to_rgb8();
        let resized = image::imageops::resize(
            &img,
            (self.cols * 2) as u32,
            (self.rows * 2) as u32,
            image::imageops::FilterType::Triangle,
        );
        let sample = |x: usize, y: usize| -> Rgb {
            let p = resized.get_pixel(x as u32, y as u32);
            (p[0], p[1], p[2])
        };
        Ok(render::compose(sample, &text, self.cols, self.rows))
    }
}

/// Render a grid to a plain-text frame (pixel glyphs → spaces). Mirrors the
/// prototype's `--dump-text`; handy for the CLI verb and tests.
pub fn grid_to_text(grid: &Grid) -> String {
    let mut out = String::new();
    for row in grid {
        let mut line = String::new();
        for cell in row {
            if cell.ch.is_empty() {
                continue; // wide continuation
            }
            let is_pixel = cell.ch.chars().all(render::is_pixel_glyph);
            if cell.ch == " " || is_pixel {
                line.push(' ');
            } else {
                line.push_str(&cell.ch);
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spawn headless Chromium directly (no box), reach it over the websocket
    /// transport (`--remote-debugging-pipe`, the production transport), and
    /// render a positioned-text fixture — asserting the DOM text lands on the
    /// exact predicted cell, i.e. full-pipeline parity with the Python
    /// prototype. Ignored (needs a browser); run with `--ignored`.
    #[test]
    #[ignore]
    fn e2e_renders_positioned_text() {
        let browser = crate::browser::launch::spawn_host_chromium(None).expect("spawn chromium");
        let sess = BrowserSession::attach(browser.cdp.clone(), 80, 30).expect("attach");

        // absolutely-positioned red text at left:160px top:128px → col 20,
        // row 8 (same geometry as the prototype fixture's REDTEXT). No '#'
        // (data: fragment terminator); body defaults to white.
        let html = "<body style='margin:0'>\
            <div style='position:absolute;left:160px;top:128px;\
            color:rgb(200,0,0);font-size:30px'>REDTEXT</div></body>";
        let url = format!("data:text/html;charset=utf-8,{}", urlencode(html));
        sess.navigate(&url).expect("navigate");
        sess.wait_load(Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(400)); // font settle

        let grid = sess.frame().expect("frame");
        let text = grid_to_text(&grid);
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines.get(8).map_or(false, |l| l.contains("REDTEXT")),
            "expected REDTEXT on row 8, got:\n{text}"
        );
        // exact column: cells 20..27 carry the glyphs
        let row8 = &grid[8];
        let word: String = (20..27).map(|c| row8[c].ch.clone()).collect();
        assert_eq!(word, "REDTEXT");
        assert_eq!(row8[20].fg, (200, 0, 0));
    }

    fn urlencode(s: &str) -> String {
        let mut out = String::new();
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char)
                }
                _ => out.push_str(&format!("%{:02X}", b)),
            }
        }
        out
    }
}
