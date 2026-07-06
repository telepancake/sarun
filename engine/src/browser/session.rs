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
use serde_json::{json, Value};

use super::cdp::Cdp;
use super::font::{inject_js, CELL_H, CELL_W};
use super::render::{self, Grid, Rgb};

const CALL: Duration = Duration::from_secs(20);

pub struct BrowserSession {
    cdp: Arc<Cdp>,
    session_id: String,
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
        let attached = cdp.call(
            "Target.attachToTarget",
            json!({ "targetId": page["targetId"], "flatten": true }),
            None,
            CALL,
        )?;
        let session_id = attached["sessionId"]
            .as_str()
            .context("attach: no sessionId")?
            .to_string();

        let me = Self { cdp, session_id, cols, rows };
        let s = Some(me.session_id.as_str());
        me.cdp.call("Page.enable", json!({}), s, CALL)?;
        me.cdp.call("DOM.enable", json!({}), s, CALL)?;
        me.cdp.call("DOMSnapshot.enable", json!({}), s, CALL)?;
        me.cdp.call(
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": cols as i64 * CELL_W,
                "height": rows as i64 * CELL_H,
                "deviceScaleFactor": 1,
                "mobile": false
            }),
            s,
            CALL,
        )?;
        me.cdp.call(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": inject_js(), "runImmediately": true }),
            s,
            CALL,
        )?;
        Ok(me)
    }

    fn s(&self) -> Option<&str> {
        Some(self.session_id.as_str())
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
        let browser = crate::browser::launch::spawn_host_chromium().expect("spawn chromium");
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
