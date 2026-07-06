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
Alt-Left/Alt-Right (or Ctrl-B/Ctrl-F) history back/forward,
Ctrl-P full-res pixel peek (sixel terminals), arrows/PgUp/PgDn scroll,
mouse click to click, everything else is forwarded to the page
(Tab focuses links, Enter follows, typing types).

Deps: websocket-client, fonttools, pillow. Chromium/Chrome/headless_shell
is found via $CELLULOSE_BROWSER or common locations.
"""

import codecs
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
import unicodedata

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
                self._fail_all()
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
            if self.closed:
                raise RuntimeError(f"{method}: connection closed")
            mid = self.next_id
            self.next_id += 1
            self.pending[mid] = (ev, slot)
        req = {"id": mid, "method": method, "params": params or {}}
        if session:
            req["sessionId"] = session
        try:
            self.ws.send(json.dumps(req))
        except Exception:
            self._fail_all()
            raise RuntimeError(f"{method}: connection closed")
        if not ev.wait(timeout):
            raise TimeoutError(f"CDP call timed out: {method}")
        msg = slot[0]
        if "error" in msg:
            raise RuntimeError(f"{method}: {msg['error'].get('message')}")
        return msg.get("result", {})

    def _fail_all(self):
        with self.lock:
            self.closed = True
            pending, self.pending = self.pending, {}
        for ev, slot in pending.values():
            slot.append({"error": {"message": "connection closed"}})
            ev.set()

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
            # resolve via the system's getaddrinfo like every other CLI tool;
            # the built-in async resolver throws DNS_PROBE_FINISHED_BAD_CONFIG
            # on systemd-resolved stubs, VPN resolv.confs, nsswitch-only hosts
            "--disable-features=AsyncDns",
            "--headless",
            "about:blank",
        ]
        proxy = os.environ.get("HTTPS_PROXY") or os.environ.get("https_proxy")
        if proxy:
            args.insert(-1, f"--proxy-server={proxy}")
        extra = os.environ.get("CELLULOSE_ARGS")
        if extra:
            args[-1:-1] = extra.split()
        self.proc = subprocess.Popen(
            args, stderr=subprocess.PIPE, stdout=subprocess.DEVNULL
        )
        try:
            ws_url = self._wait_for_ws()
            self.cdp = CDP(ws_url)
            self.session = self._attach_first_page()
            self._setup()
        except BaseException:
            self.proc.kill()
            shutil.rmtree(self.profile, ignore_errors=True)
            raise

    def _wait_for_ws(self):
        fd = self.proc.stderr.fileno()
        deadline = time.time() + 30
        buf = b""
        while time.time() < deadline:
            if not select.select([fd], [], [], 0.5)[0]:
                continue
            chunk = os.read(fd, 4096)
            if not chunk:
                break
            buf += chunk
            m = re.search(rb"DevTools listening on (ws://\S+)", buf)
            if m:
                # keep draining stderr so chromium never blocks on a full pipe
                threading.Thread(
                    target=lambda: [None for _ in self.proc.stderr], daemon=True
                ).start()
                return m.group(1).decode()
        raise RuntimeError("cellulose: browser did not expose a DevTools socket")

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
        raise RuntimeError("cellulose: no page target appeared")

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
        if not re.match(r"[a-z][a-z0-9+.-]*:", url):  # data:, about:, file://…
            url = "https://" + url
        res = self.cdp.call("Page.navigate", {"url": url}, session=self.session)
        return res.get("errorText")

    def wait_load(self, timeout=15):
        deadline = time.time() + timeout
        while time.time() < deadline:
            for ev in self.cdp.drain_events():
                if ev.get("method") == "Page.loadEventFired":
                    return True
            time.sleep(0.05)
        return False

    def screenshot(self, full=False):
        import base64

        data = self.cdp.call(
            "Page.captureScreenshot", {"format": "png"}, session=self.session
        )["data"]
        return self.decode_frame(base64.b64decode(data), full)

    def decode_frame(self, png_bytes, full=False):
        from PIL import Image

        img = Image.open(io.BytesIO(png_bytes)).convert("RGB")
        if full:
            return img
        # 2x2 color samples per cell for quadrant-block rendering
        return img.resize((self.cols * 2, self.rows * 2), Image.BOX)

    def start_screencast(self):
        """Push-based frames: Chromium sends Page.screencastFrame only when
        the compositor produced a new frame — no blind polling."""
        self.cdp.call(
            "Page.startScreencast",
            {"format": "png", "everyNthFrame": 1},
            session=self.session,
        )

    def ack_frame(self, frame_session):
        try:
            self.cdp.call(
                "Page.screencastFrameAck",
                {"sessionId": frame_session},
                session=self.session,
            )
        except RuntimeError:
            pass  # frame from a navigation that already ended

    def history_go(self, delta):
        hist = self.cdp.call("Page.getNavigationHistory", session=self.session)
        i = hist["currentIndex"] + delta
        if 0 <= i < len(hist["entries"]):
            self.cdp.call(
                "Page.navigateToHistoryEntry",
                {"entryId": hist["entries"][i]["id"]},
                session=self.session,
            )

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
            # start/length are UTF-16 code units (CDP InlineTextBox semantics)
            seg = (
                strings[ti]
                .encode("utf-16-le", "surrogatepass")[2 * start : 2 * (start + length)]
                .decode("utf-16-le", "surrogatepass")
            )
            for ch in seg:
                cells = char_cells(ch)
                if col + cells > self.cols:
                    break
                # controls/escapes must never reach the terminal; formatting
                # chars, lone surrogates, and combining marks would occupy a
                # layout cell (the cell font gives everything an advance) but
                # print zero-or-other width, shifting the row
                if unicodedata.category(ch) in ("Cc", "Cf", "Cs", "Mn", "Me"):
                    ch = " "
                out.append((row, col, ch, color))
                col += cells
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


# quadrant blocks indexed by pixel bitmask: bit 1 = upper-left, 2 = upper-
# right, 4 = lower-left, 8 = lower-right lit (drawn in the foreground color)
QUADS = " ▘▝▀▖▌▞▛▗▚▐▜▄▙▟█"
PIXEL_CHARS = frozenset(QUADS) - {" "}


def _dist2(a, b):
    return (a[0] - b[0]) ** 2 + (a[1] - b[1]) ** 2 + (a[2] - b[2]) ** 2


def _quad_cell(pix):
    """4 RGB samples (UL, UR, LL, LR) -> (char, fg, bg): pick the 2-color
    partition seeded by the most distant pair, render lit quads as fg."""
    seed_a, seed_b, worst = pix[0], pix[0], -1
    for i in range(4):
        for j in range(i + 1, 4):
            d = _dist2(pix[i], pix[j])
            if d > worst:
                worst, seed_a, seed_b = d, pix[i], pix[j]
    if worst == 0:
        return " ", pix[0], pix[0]
    bits, on, off = 0, [], []
    for k, p in enumerate(pix):
        if _dist2(p, seed_a) <= _dist2(p, seed_b):
            bits |= 1 << k
            on.append(p)
        else:
            off.append(p)
    avg = lambda ps: tuple(sum(c[i] for c in ps) // len(ps) for i in range(3))
    return QUADS[bits], avg(on), avg(off)


def compose_frame(browser, img=None):
    """-> grid[rows][cols] of (char, fg, bg) cells."""
    if img is None:
        img = browser.screenshot()
    px = img.load()
    rows, cols = browser.rows, browser.cols
    grid = [
        [
            _quad_cell(
                (px[2 * c, 2 * r], px[2 * c + 1, 2 * r],
                 px[2 * c, 2 * r + 1], px[2 * c + 1, 2 * r + 1])
            )
            for c in range(cols)
        ]
        for r in range(rows)
    ]
    for row, col, ch, color in browser.snapshot_text():
        if 0 <= row < rows and 0 <= col < cols:
            p = (px[2 * col, 2 * row], px[2 * col + 1, 2 * row],
                 px[2 * col, 2 * row + 1], px[2 * col + 1, 2 * row + 1])
            bg = tuple(sum(c[i] for c in p) // 4 for i in range(3))
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
            ch if ch and ch not in PIXEL_CHARS else (" " if ch else "")
            for ch, _, _ in row
        )
        out.write(line.rstrip() + "\n")


def interactive(browser):
    import base64
    import termios
    import tty

    fd = sys.stdin.fileno()
    old = termios.tcgetattr(fd)
    out = sys.stdout
    out.write("\x1b[?1049h\x1b[?25l\x1b[?1002h\x1b[?1006h")
    dirty = True
    frame = None  # latest screencast png bytes
    prev_lines = None  # last rendered rows, for diff redraws
    url_edit = None  # None = browsing; str = editing URL
    buf = b""
    decoder = codecs.getincrementaldecoder("utf-8")("replace")
    browser.start_screencast()
    try:
        tty.setraw(fd)
        while True:
            if dirty:
                prev_lines = draw_screen(browser, out, url_edit, frame, prev_lines)
                dirty = False
            # block on tty input OR wake shortly to pick up screencast frames
            ready, _, _ = select.select([fd], [], [], 0.1)
            for ev in browser.cdp.drain_events():
                meth = ev.get("method")
                if meth == "Page.screencastFrame":
                    p = ev["params"]
                    browser.ack_frame(p["sessionId"])
                    frame = base64.b64decode(p["data"])
                    dirty = True
                elif meth in ("Page.loadEventFired", "Page.frameNavigated"):
                    browser.start_screencast()  # navigation stops the cast
                    dirty = True
            if not ready:
                continue
            buf += os.read(fd, 4096)
            dirty = True
            while buf:
                tok, buf = next_token(buf)
                if tok is None:
                    break  # incomplete sequence, wait for more bytes
                if url_edit is not None:
                    url_edit = handle_url_edit(browser, url_edit, tok)
                    continue
                if tok == b"\x11":  # Ctrl-Q
                    return
                if tok == b"\x0c":  # Ctrl-L
                    url_edit = ""
                elif tok == b"\x12":  # Ctrl-R
                    browser.cdp.call("Page.reload", session=browser.session)
                elif tok == b"\x10":  # Ctrl-P: sixel pixel peek
                    sixel_peek(browser, out, fd)
                    prev_lines = None  # force full repaint
                elif tok in (b"\x1b[1;3D", b"\x1b\x1b[D", b"\x02"):  # Alt-Left/^B
                    browser.history_go(-1)
                elif tok in (b"\x1b[1;3C", b"\x1b\x1b[C", b"\x06"):  # Alt-Right/^F
                    browser.history_go(1)
                elif tok == b"\x1b[A":
                    browser.scroll(-3)
                elif tok == b"\x1b[B":
                    browser.scroll(3)
                elif tok == b"\x1b[5~":
                    browser.scroll(-(browser.rows - 2))
                elif tok == b"\x1b[6~":
                    browser.scroll(browser.rows - 2)
                elif tok.startswith(b"\x1b[<"):
                    m = re.match(rb"\x1b\[<(\d+);(\d+);(\d+)([mM])", tok)
                    if m and m.group(4) == b"M" and int(m.group(1)) == 0:
                        browser.click(int(m.group(2)) - 1, int(m.group(3)) - 1)
                elif tok == b"\r":
                    browser.key("Enter", "Enter", 13, "\r")
                elif tok == b"\t":
                    browser.key("Tab", "Tab", 9)
                elif tok == b"\x7f":
                    browser.key("Backspace", "Backspace", 8)
                elif not tok.startswith(b"\x1b"):
                    text = decoder.decode(tok)
                    if text:
                        browser.type_text(text)
    finally:
        termios.tcsetattr(fd, termios.TCSADRAIN, old)
        out.write("\x1b[?1006l\x1b[?1002l\x1b[?25h\x1b[?1049l")
        out.flush()


def next_token(buf):
    """Split one input token off the front of the tty byte buffer.

    Tokens: a single control byte, one complete escape sequence, or a run
    of printable bytes (possibly a partial UTF-8 char — the incremental
    decoder downstream handles splits). Returns (None, buf) when the buffer
    holds only an incomplete escape sequence.
    """
    if buf[0:1] == b"\x1b":
        if buf == b"\x1b":
            return buf, b""  # a lone ESC press
        if buf[1:2] == b"[":
            m = re.match(rb"\x1b\[[0-9;<]*[@-~]", buf)
            if m:
                return m.group(0), buf[m.end():]
            if len(buf) < 32:
                return None, buf  # CSI sequence still arriving
            return buf[:1], buf[1:]  # garbage; drop the ESC
        return buf[:2], buf[2:]  # alt-modified key
    if buf[0] < 0x20 or buf[0] == 0x7F:
        return buf[:1], buf[1:]
    n = 1
    while n < len(buf) and buf[n] != 0x1B and not (buf[n] < 0x20 or buf[n] == 0x7F):
        n += 1
    return buf[:n], buf[n:]


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


def row_ansi(row):
    parts = []
    last_fg = last_bg = None
    for ch, fg, bg in row:
        if fg != last_fg:
            parts.append("\x1b[38;2;%d;%d;%dm" % fg)
            last_fg = fg
        if bg != last_bg:
            parts.append("\x1b[48;2;%d;%d;%dm" % bg)
            last_bg = bg
        parts.append(ch)
    parts.append("\x1b[0m")
    return "".join(parts)


def draw_screen(browser, out, url_edit, frame=None, prev_lines=None):
    """Render; when prev_lines is given, repaint only rows that changed.
    Returns this frame's rows for the next diff."""
    img = browser.decode_frame(frame) if frame else None
    grid = compose_frame(browser, img)
    url, title = browser.url_and_title()
    if url_edit is not None:
        status = f" url: {url_edit}_"
    else:
        status = (f" {title}  {url}  [^L url ^R reload alt-←→ history"
                  f" ^P pixels ^Q quit]")
    status = "".join(c if c >= " " else " " for c in status)  # page-controlled
    status = status[: browser.cols].ljust(browser.cols)
    lines = [row_ansi(r) for r in grid]
    lines.append("\x1b[7m" + status + "\x1b[0m")
    for i, line in enumerate(lines):
        if prev_lines is not None and i < len(prev_lines) and prev_lines[i] == line:
            continue
        out.write(f"\x1b[{i + 1};1H" + line)
    out.flush()
    return lines


def sixel_encode(img):
    """PIL RGB image -> DECSIXEL escape string (256-color adaptive palette)."""
    from PIL import Image

    q = img.convert("RGB").quantize(colors=256, dither=Image.Dither.NONE)
    pal = q.getpalette()[: 256 * 3]
    w, h = q.size
    px = q.load()
    out = ['\x1bPq"1;1;%d;%d' % (w, h)]
    for i in range(256):
        r, g, b = pal[3 * i : 3 * i + 3] or [0, 0, 0]
        out.append("#%d;2;%d;%d;%d" % (i, r * 100 // 255, g * 100 // 255,
                                       b * 100 // 255))
    for band in range(0, h, 6):
        rows = min(6, h - band)
        per_color = {}
        for y in range(rows):
            bit = 1 << y
            for x in range(w):
                c = px[x, band + y]
                arr = per_color.get(c)
                if arr is None:
                    arr = per_color[c] = bytearray(w)
                arr[x] |= bit
        first = True
        for c, bits in per_color.items():
            if not first:
                out.append("$")  # carriage return within the band
            first = False
            out.append("#%d" % c)
            run_ch, run_n = None, 0
            for x in range(w):
                ch = chr(63 + bits[x])
                if ch == run_ch:
                    run_n += 1
                else:
                    if run_ch is not None:
                        out.append(run_ch if run_n == 1 else "!%d%s" % (run_n, run_ch))
                    run_ch, run_n = ch, 1
            out.append(run_ch if run_n == 1 else "!%d%s" % (run_n, run_ch))
        out.append("-")  # next band
    out.append("\x1b\\")
    return "".join(out)


def sixel_peek(browser, out, fd):
    """Ctrl-P: blit the real full-resolution viewport as a sixel image and
    hold it until the next keypress. Pixels for terminals that have them."""
    img = browser.screenshot(full=True)
    out.write("\x1b[H\x1b[2J" + sixel_encode(img))
    out.flush()
    select.select([fd], [], [])  # any key returns to the cell view
    os.read(fd, 4096)
    out.write("\x1b[2J")


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
    status = 0
    try:
        err = browser.navigate(url)
        browser.wait_load()
        time.sleep(0.3)  # let injected style settle and fonts apply
        if err:
            print(f"cellulose: navigation failed: {err}", file=sys.stderr)
            status = 2
        if mode == "interactive":
            if not sys.stdin.isatty():
                sys.exit("cellulose: interactive mode needs a tty; try --dump")
            interactive(browser)
        else:
            grid = compose_frame(browser)
            try:
                (render_ansi if mode == "dump" else render_text)(grid, sys.stdout)
            except BrokenPipeError:
                # reader (e.g. `| head`) went away; not an error
                os.dup2(os.open(os.devnull, os.O_WRONLY), sys.stdout.fileno())
    finally:
        browser.close()
    sys.exit(status)


if __name__ == "__main__":
    main()
