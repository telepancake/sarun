#!/usr/bin/env -S uv run --quiet --with websocket-client,fonttools,pillow python3
"""cellulose — a textmode browser over stock headless Chromium + CDP.

carbonyl's design, rebuilt on public API only: no Chromium patches, no fork.

- A synthetic font (cellfont.py) with empty glyphs and exact cell-multiple
  advances is forced on every document, so text layout lands on a terminal
  grid and screenshots contain only non-text pixels.
- Text (content, position, color) comes from DOMSnapshot.captureSnapshot.
- Non-text pixels come from Page.captureScreenshot, downscaled to two
  vertical samples per cell and drawn with U+2584 half-blocks.
- Input is forwarded with Input.dispatchKeyEvent / dispatchMouseEvent /
  insertText; terminal mouse clicks map cell -> CSS px.

Usage:
    ./cellulose.py URL                     # interactive (needs a real tty)
    ./cellulose.py --dump URL              # render one ANSI frame to stdout
    ./cellulose.py --dump-text URL         # render one plain-text frame
    ./cellulose.py --size 100x40 URL       # override terminal size

Interactive keys: Ctrl-Q quit, Ctrl-L edit URL, Ctrl-R reload,
arrows/PgUp/PgDn scroll, mouse click to click, everything else is
forwarded to the page (Tab focuses links, Enter follows, typing types).

Deps: websocket-client, fonttools, pillow. Chromium/Chrome/headless_shell
is found via $CELLULOSE_BROWSER or common locations.
"""

import io
import json
import os
import re
import select
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from cellfont import char_cells, font_data_url

CELL_W = 8  # CSS px per terminal column
CELL_H = 16  # CSS px per terminal row

FORCED_CSS_TEMPLATE = """
@font-face { font-family: '__cellulose'; src: url(%(font)s); }
* {
    font-family: '__cellulose' !important;
    font-size: %(h)dpx !important;
    line-height: %(h)dpx !important;
    letter-spacing: 0 !important;
    word-spacing: 0 !important;
    text-indent: 0 !important;
    text-align: left !important;
    text-justify: none !important;
    font-kerning: none !important;
    font-variant-ligatures: none !important;
    font-feature-settings: normal !important;
    text-shadow: none !important;
    text-transform: none !important;
    vertical-align: baseline !important;
    animation: none !important;
    transition: none !important;
    scroll-behavior: auto !important;
    border-radius: 0 !important;
}
"""

INJECT_JS_TEMPLATE = """
(() => {
    const css = %(css_json)s;
    const add = () => {
        const root = document.documentElement;
        if (!root) return false;
        const s = document.createElement('style');
        s.id = '__cellulose_style';
        s.textContent = css;
        root.appendChild(s);
        return true;
    };
    if (!add()) {
        new MutationObserver((_, obs) => { if (add()) obs.disconnect(); })
            .observe(document, { childList: true, subtree: true });
    }
})();
"""

BROWSER_CANDIDATES = [
    os.environ.get("CELLULOSE_BROWSER"),
    # full chromium first: headless_shell has no enterprise-policy machinery,
    # and the EncryptedClientHelloEnabled=false policy is the only way to get
    # TLS through ECH-intolerant MITM proxies (see README)
    "/opt/pw-browsers/chromium-1194/chrome-linux/chrome",
    "/opt/pw-browsers/chromium_headless_shell-1194/chrome-linux/headless_shell",
    shutil.which("chromium"),
    shutil.which("chromium-browser"),
    shutil.which("google-chrome"),
    shutil.which("headless_shell"),
]


def find_browser():
    for path in BROWSER_CANDIDATES:
        if path and os.path.exists(path):
            return path
    sys.exit("cellulose: no Chromium found; set $CELLULOSE_BROWSER")


class CDP:
    """Minimal synchronous CDP client over the browser websocket.

    All commands go through the browser connection with flat session
    routing (Target.attachToTarget flatten=true). A reader thread parses
    frames; command callers block on their id, events queue up per session.
    """

    def __init__(self, ws_url):
        import websocket

        self.ws = websocket.create_connection(
            ws_url, enable_multithread=True, suppress_origin=True
        )
        self.next_id = 1
        self.pending = {}  # id -> [event, result-or-None]
        self.events = []
        self.lock = threading.Lock()
        self.closed = False
        self.reader = threading.Thread(target=self._read_loop, daemon=True)
        self.reader.start()

    def _read_loop(self):
        while not self.closed:
            try:
                msg = json.loads(self.ws.recv())
            except Exception:
                self.closed = True
                with self.lock:
                    for ev, slot in self.pending.values():
                        slot.append({"error": {"message": "connection closed"}})
                        ev.set()
                return
            if "id" in msg:
                with self.lock:
                    entry = self.pending.pop(msg["id"], None)
                if entry:
                    entry[1].append(msg)
                    entry[0].set()
            else:
                with self.lock:
                    self.events.append(msg)

    def call(self, method, params=None, session=None, timeout=30):
        ev = threading.Event()
        slot = []
        with self.lock:
            mid = self.next_id
            self.next_id += 1
            self.pending[mid] = (ev, slot)
        req = {"id": mid, "method": method, "params": params or {}}
        if session:
            req["sessionId"] = session
        self.ws.send(json.dumps(req))
        if not ev.wait(timeout):
            raise TimeoutError(f"CDP call timed out: {method}")
        msg = slot[0]
        if "error" in msg:
            raise RuntimeError(f"{method}: {msg['error'].get('message')}")
        return msg.get("result", {})

    def drain_events(self):
        with self.lock:
            evs, self.events = self.events, []
        return evs

    def close(self):
        self.closed = True
        try:
            self.ws.close()
        except Exception:
            pass


class Browser:
    def __init__(self, cols, rows):
        self.cols, self.rows = cols, rows
        self.profile = tempfile.mkdtemp(prefix="cellulose-")
        args = [
            find_browser(),
            "--remote-debugging-port=0",
            f"--user-data-dir={self.profile}",
            "--no-sandbox",
            "--no-first-run",
            "--disable-dev-shm-usage",
            "--disable-extensions",
            "--disable-smooth-scrolling",
            "--hide-scrollbars",
            "--mute-audio",
            "--force-color-profile=srgb",
            "--headless",
            "about:blank",
        ]
        proxy = os.environ.get("HTTPS_PROXY") or os.environ.get("https_proxy")
        if proxy:
            args.insert(-1, f"--proxy-server={proxy}")
        self.proc = subprocess.Popen(
            args, stderr=subprocess.PIPE, stdout=subprocess.DEVNULL
        )
        ws_url = self._wait_for_ws()
        self.cdp = CDP(ws_url)
        self.session = self._attach_first_page()
        self._setup()

    def _wait_for_ws(self):
        deadline = time.time() + 30
        for line in self.proc.stderr:
            m = re.search(rb"DevTools listening on (ws://\S+)", line)
            if m:
                # stop consuming stderr so chromium never blocks on a full pipe
                threading.Thread(
                    target=lambda: [None for _ in self.proc.stderr], daemon=True
                ).start()
                return m.group(1).decode()
            if time.time() > deadline:
                break
        sys.exit("cellulose: browser did not expose a DevTools socket")

    def _attach_first_page(self):
        for _ in range(100):
            targets = self.cdp.call("Target.getTargets")["targetInfos"]
            pages = [t for t in targets if t["type"] == "page"]
            if pages:
                return self.cdp.call(
                    "Target.attachToTarget",
                    {"targetId": pages[0]["targetId"], "flatten": True},
                )["sessionId"]
            time.sleep(0.05)
        sys.exit("cellulose: no page target appeared")

    def _setup(self):
        s = self.session
        call = self.cdp.call
        call("Page.enable", session=s)
        call("DOM.enable", session=s)
        call("DOMSnapshot.enable", session=s)
        call(
            "Emulation.setDeviceMetricsOverride",
            {
                "width": self.cols * CELL_W,
                "height": self.rows * CELL_H,
                "deviceScaleFactor": 1,
                "mobile": False,
            },
            session=s,
        )
        call("Emulation.setFocusEmulationEnabled", {"enabled": True}, session=s)
        css = FORCED_CSS_TEMPLATE % {"font": font_data_url(), "h": CELL_H}
        call(
            "Page.addScriptToEvaluateOnNewDocument",
            {"source": INJECT_JS_TEMPLATE % {"css_json": json.dumps(css)},
             "runImmediately": True},
            session=s,
        )

    def navigate(self, url):
        if "://" not in url:
            url = "https://" + url
        self.cdp.call("Page.navigate", {"url": url}, session=self.session)

    def wait_load(self, timeout=15):
        deadline = time.time() + timeout
        while time.time() < deadline:
            for ev in self.cdp.drain_events():
                if ev.get("method") == "Page.loadEventFired":
                    return True
            time.sleep(0.05)
        return False

    def screenshot(self):
        from PIL import Image

        data = self.cdp.call(
            "Page.captureScreenshot", {"format": "png"}, session=self.session
        )["data"]
        import base64

        img = Image.open(io.BytesIO(base64.b64decode(data))).convert("RGB")
        # two vertical color samples per cell for half-block rendering
        return img.resize((self.cols, self.rows * 2), Image.BOX)

    def snapshot_text(self):
        """-> list of (row, col, char, (r,g,b)) from the DOM snapshot."""
        res = self.cdp.call(
            "DOMSnapshot.captureSnapshot",
            {"computedStyles": ["color", "visibility", "opacity"]},
            session=self.session,
        )
        strings = res["strings"]
        out = []
        doc = res["documents"][0]  # main frame only (v1)
        layout = doc["layout"]
        text_idx = layout.get("text", [])
        styles = layout.get("styles", [])
        sx = doc.get("scrollOffsetX", 0)
        sy = doc.get("scrollOffsetY", 0)
        tb = doc.get("textBoxes", {})
        for li, bounds, start, length in zip(
            tb.get("layoutIndex", []),
            tb.get("bounds", []),
            tb.get("start", []),
            tb.get("length", []),
        ):
            ti = text_idx[li] if li < len(text_idx) else -1
            if ti < 0:
                continue
            color = (0, 0, 0)
            visible = True
            if li < len(styles) and styles[li]:
                st = [strings[i] if i >= 0 else "" for i in styles[li]]
                color = parse_css_color(st[0]) or color
                if len(st) > 1 and st[1] == "hidden":
                    visible = False
                if len(st) > 2 and st[2]:
                    try:
                        if float(st[2]) < 0.05:
                            visible = False
                    except ValueError:
                        pass
            if not visible:
                continue
            x, y, w, h = bounds
            row = int((y - sy + h / 2) // CELL_H)
            col = int(round((x - sx) / CELL_W))
            if row < 0 or row >= self.rows:
                continue
            for ch in strings[ti][start : start + length]:
                if col >= self.cols:
                    break
                if ch not in ("\n", "\r", "\t"):
                    out.append((row, col, ch, color))
                col += char_cells(ch)
        return out

    def scroll(self, dy_cells):
        self.cdp.call(
            "Input.dispatchMouseEvent",
            {
                "type": "mouseWheel",
                "x": self.cols * CELL_W // 2,
                "y": self.rows * CELL_H // 2,
                "deltaX": 0,
                "deltaY": dy_cells * CELL_H,
            },
            session=self.session,
        )

    def click(self, col, row):
        x, y = int((col + 0.5) * CELL_W), int((row + 0.5) * CELL_H)
        for typ, count in (("mousePressed", 1), ("mouseReleased", 1)):
            self.cdp.call(
                "Input.dispatchMouseEvent",
                {"type": typ, "x": x, "y": y, "button": "left",
                 "clickCount": count},
                session=self.session,
            )

    def type_text(self, text):
        self.cdp.call("Input.insertText", {"text": text}, session=self.session)

    def key(self, key, code, vk, text=""):
        base = {"key": key, "code": code, "windowsVirtualKeyCode": vk,
                "nativeVirtualKeyCode": vk}
        down = dict(base, type="rawKeyDown")
        if text:
            down.update(type="keyDown", text=text, unmodifiedText=text)
        self.cdp.call("Input.dispatchKeyEvent", down, session=self.session)
        self.cdp.call("Input.dispatchKeyEvent", dict(base, type="keyUp"),
                      session=self.session)

    def url_and_title(self):
        try:
            hist = self.cdp.call("Page.getNavigationHistory", session=self.session)
            entry = hist["entries"][hist["currentIndex"]]
            return entry.get("url", ""), entry.get("title", "")
        except Exception:
            return "", ""

    def close(self):
        self.cdp.close()
        self.proc.terminate()
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()
        shutil.rmtree(self.profile, ignore_errors=True)


def parse_css_color(s):
    m = re.match(r"rgba?\((\d+),\s*(\d+),\s*(\d+)(?:,\s*([\d.]+))?\)", s or "")
    if not m:
        return None
    if m.group(4) is not None and float(m.group(4)) < 0.05:
        return None
    return (int(m.group(1)), int(m.group(2)), int(m.group(3)))


def compose_frame(browser):
    """-> grid[rows][cols] of (char, fg, bg) cells."""
    img = browser.screenshot()
    px = img.load()
    rows, cols = browser.rows, browser.cols
    grid = [
        [("▄", px[c, r * 2 + 1], px[c, r * 2]) for c in range(cols)]
        for r in range(rows)
    ]
    for row, col, ch, color in browser.snapshot_text():
        if 0 <= row < rows and 0 <= col < cols:
            bg = px[col, row * 2]
            grid[row][col] = (ch, color, bg)
            if char_cells(ch) == 2 and col + 1 < cols:
                grid[row][col + 1] = ("", color, bg)  # continuation cell
    return grid


def render_ansi(grid, out):
    last_fg = last_bg = None
    for row in grid:
        for ch, fg, bg in row:
            if fg != last_fg:
                out.write("\x1b[38;2;%d;%d;%dm" % fg)
                last_fg = fg
            if bg != last_bg:
                out.write("\x1b[48;2;%d;%d;%dm" % bg)
                last_bg = bg
            out.write(ch)
        out.write("\x1b[0m\n")
        last_fg = last_bg = None


def render_text(grid, out):
    for row in grid:
        line = "".join(
            ch if ch and ch != "▄" else (" " if ch else "") for ch, _, _ in row
        )
        out.write(line.rstrip() + "\n")


def interactive(browser):
    import termios
    import tty

    fd = sys.stdin.fileno()
    old = termios.tcgetattr(fd)
    out = sys.stdout
    out.write("\x1b[?1049h\x1b[?25l\x1b[?1002h\x1b[?1006h")
    dirty = True
    url_edit = None  # None = browsing; str = editing URL
    try:
        tty.setraw(fd)
        while True:
            if dirty:
                draw_screen(browser, out, url_edit)
                dirty = False
            ready, _, _ = select.select([fd], [], [], 0.6)
            for ev in browser.cdp.drain_events():
                if ev.get("method") in (
                    "Page.loadEventFired",
                    "Page.frameNavigated",
                ):
                    dirty = True
            if not ready:
                dirty = True  # periodic refresh picks up page changes
                continue
            data = os.read(fd, 64)
            dirty = True
            if url_edit is not None:
                url_edit = handle_url_edit(browser, url_edit, data)
                continue
            if data == b"\x11":  # Ctrl-Q
                return
            if data == b"\x0c":  # Ctrl-L
                url_edit = ""
            elif data == b"\x12":  # Ctrl-R
                browser.cdp.call("Page.reload", session=browser.session)
            elif data == b"\x1b[A":
                browser.scroll(-3)
            elif data == b"\x1b[B":
                browser.scroll(3)
            elif data == b"\x1b[5~":
                browser.scroll(-(browser.rows - 2))
            elif data == b"\x1b[6~":
                browser.scroll(browser.rows - 2)
            elif data.startswith(b"\x1b[<"):
                m = re.match(rb"\x1b\[<(\d+);(\d+);(\d+)([mM])", data)
                if m and m.group(4) == b"M" and int(m.group(1)) == 0:
                    browser.click(int(m.group(2)) - 1, int(m.group(3)) - 1)
            elif data == b"\r":
                browser.key("Enter", "Enter", 13, "\r")
            elif data == b"\t":
                browser.key("Tab", "Tab", 9)
            elif data == b"\x7f":
                browser.key("Backspace", "Backspace", 8)
            elif not data.startswith(b"\x1b"):
                browser.type_text(data.decode("utf-8", "replace"))
    finally:
        termios.tcsetattr(fd, termios.TCSADRAIN, old)
        out.write("\x1b[?1006l\x1b[?1002l\x1b[?25h\x1b[?1049l")
        out.flush()


def handle_url_edit(browser, buf, data):
    if data == b"\r":
        if buf:
            browser.navigate(buf)
        return None
    if data in (b"\x1b", b"\x11"):
        return None
    if data == b"\x7f":
        return buf[:-1]
    if not data.startswith(b"\x1b"):
        return buf + data.decode("utf-8", "replace")
    return buf


def draw_screen(browser, out, url_edit):
    grid = compose_frame(browser)
    buf = io.StringIO()
    render_ansi(grid, buf)
    url, title = browser.url_and_title()
    if url_edit is not None:
        status = f" url: {url_edit}_"
    else:
        status = f" {title}  {url}  [^L url ^R reload ^Q quit]"
    status = status[: browser.cols].ljust(browser.cols)
    out.write("\x1b[H" + buf.getvalue().replace("\n", "\r\n"))
    out.write("\x1b[7m" + status + "\x1b[0m")
    out.flush()


def main():
    args = sys.argv[1:]
    mode = "interactive"
    size = None
    while args and args[0].startswith("--"):
        if args[0] == "--dump":
            mode = "dump"
        elif args[0] == "--dump-text":
            mode = "dump-text"
        elif args[0] == "--size":
            size = args[1]
            args = args[1:]
        else:
            sys.exit(f"cellulose: unknown flag {args[0]}")
        args = args[1:]
    if not args:
        sys.exit(__doc__.strip())
    url = args[0]

    if size:
        cols, rows = (int(v) for v in size.split("x"))
    elif mode == "interactive":
        ts = shutil.get_terminal_size()
        cols, rows = ts.columns, ts.lines - 1  # one row for the status bar
    else:
        cols, rows = 100, 36

    browser = Browser(cols, rows)
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    try:
        browser.navigate(url)
        browser.wait_load()
        time.sleep(0.3)  # let injected style settle and fonts apply
        if mode == "interactive":
            if not sys.stdin.isatty():
                sys.exit("cellulose: interactive mode needs a tty; try --dump")
            interactive(browser)
        else:
            grid = compose_frame(browser)
            (render_ansi if mode == "dump" else render_text)(grid, sys.stdout)
    finally:
        browser.close()


if __name__ == "__main__":
    main()
