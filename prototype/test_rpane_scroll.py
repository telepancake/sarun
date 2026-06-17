#!/usr/bin/env python3
"""Focused regression test for the right-pane docs being scrollable.

Loads the real UI class from `sarun`, neutralises the heavy on_mount work
(overlay mount / server / timers) so the test needs neither bwrap nor pyfuse3,
shrinks the terminal so the 80-line doc block overflows the pane, and asserts
that the docs container (#box-wrap) is a scrollable VerticalScroll that actually
has somewhere to scroll to.
"""
import os, sys, asyncio, tempfile
from pathlib import Path
from importlib.machinery import SourceFileLoader

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)

# isolate XDG so loading rules stores touches nothing real
tmp = Path(tempfile.mkdtemp(prefix="rpane-scroll-"))
os.environ["XDG_STATE_HOME"]  = str(tmp/"state")
os.environ["XDG_RUNTIME_DIR"] = str(tmp/"run")
os.environ["XDG_CONFIG_HOME"] = str(tmp/"config")
os.environ["XDG_DATA_HOME"]   = str(tmp/"data")
for p in ("state","run","config","data"):
    (tmp/p).mkdir(parents=True, exist_ok=True)

m = SourceFileLoader("sarun", str(Path(__file__).parent/"sarun")).load_module()

from textual.containers import VerticalScroll

async def drive():
    UI = m._make_ui_app()
    # neutralise the real startup: no overlay mount, no server, no timers.
    async def _noop_mount(self):
        self.query_one("#s-tab").add_columns("F","Name","PID","Cmd","Age")
        self.query_one("#cf-tab").add_columns("","Path","Size")
        self.query_one("#pr-tab").add_columns("TGID","PPID","Exe","Argv")
        self._set_view("boxes")
    UI.on_mount = _noop_mount
    app = UI()
    # small viewport so the docs overflow for sure
    async with app.run_test(size=(100, 15)) as pilot:
        await pilot.pause()
        await pilot.press("b"); await pilot.pause()
        check(app.view == "boxes", "boxes view selected")

        wrap = app.query_one("#box-wrap")
        check(isinstance(wrap, VerticalScroll),
              "#box-wrap is a VerticalScroll container")
        check(app.RIGHT["boxes"] == "#box-wrap",
              "RIGHT map points 'boxes' at the scroll wrapper")

        # no box selected -> docs are shown in #box-detail
        app._update_box_detail(); await pilot.pause()
        detail = app.query_one("#box-detail")
        doc = m.collect_docs()
        check(bool(doc) and len(doc.splitlines()) > 15,
              f"collect_docs() returns {len(doc.splitlines())} lines (overflows a 15-row pane)")

        # the wrapper must have somewhere to scroll, and be able to
        check(wrap.max_scroll_y > 0,
              f"#box-wrap can scroll (max_scroll_y={wrap.max_scroll_y})")
        check(wrap.allow_vertical_scroll, "#box-wrap allows vertical scroll")

        # actually scroll it and confirm the offset moves
        wrap.scroll_end(animate=False); await pilot.pause()
        check(wrap.scroll_offset.y > 0,
              f"scrolling moves the offset (y={wrap.scroll_offset.y})")
    return

def main():
    try:
        asyncio.run(drive())
    finally:
        import shutil; shutil.rmtree(tmp, ignore_errors=True)
    if _fails:
        print(f"\n{len(_fails)} FAILED"); sys.exit(1)
    print("\nall passed"); sys.exit(0)

if __name__ == "__main__":
    main()
