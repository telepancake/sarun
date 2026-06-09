#!/usr/bin/env python3
"""Headless UI smoke: drive the Textual app with app.run_test(), confirm all five
views render, rules/file-rules edit, and the overlay mount comes up and tears down
cleanly. Run:
    /home/user/venv/bin/python test_ui_smoke.py

Self-safety: XDG dirs are redirected to a temp tree so the real config/state is
untouched; the overlay mounts at the temp runtime mnt and is lazy-unmounted on exit.
"""
import os, sys, asyncio, tempfile, shutil, subprocess
from pathlib import Path
from importlib.machinery import SourceFileLoader

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


async def drive(m):
    UI = None
    # the App class is defined inside run_ui(); reach it by constructing via run_ui's
    # closure is awkward, so we replicate the entry: call the module's UI factory.
    app = m._make_ui_app()()
    async with app.run_test() as pilot:
        await pilot.pause()
        # cycle through the five views
        for key, name in (("b","boxes"),("c","changes"),("t","netlog"),
                          ("w","net"),("f","file")):
            await pilot.press(key); await pilot.pause()
            check(app.view==name, f"view '{name}' selected via key '{key}'")
        # add a network rule via the store + refresh
        app.rules_store.insert(m.Rule.single("allow","host","example.com"))
        app._refresh_rules(); await pilot.pause()
        check(any(r.clauses[0].match.pattern=="example.com" for r in app.rules_store.rules),
              "network rule added")
        # add a file rule
        app.frules_store.insert(m.FileRule.single("discard","path","*.log"))
        app._refresh_file_rules(); await pilot.pause()
        check(any(r.clauses[0].match.pattern=="*.log" for r in app.frules_store.rules),
              "file rule added")
        # the mount should have come up
        check(app.overlay_mount.ops is not None, "overlay mount ops object created")
        check(os.path.ismount(str(app.overlay_mount.mountpoint)), "overlay is mounted")
        # the synthetic root lists no sessions at rest
        listing = subprocess.run(["timeout","10","ls",str(app.overlay_mount.mountpoint)],
                                 capture_output=True, text=True)
        check(listing.returncode==0 and listing.stdout.strip()=="",
              "overlay synthetic root is empty at rest")
        await pilot.pause()
    # after exit, the mount must be gone
    check(not os.path.ismount(str(app.overlay_mount.mountpoint)),
          "overlay unmounted on app exit")


def main():
    tmp = Path(tempfile.mkdtemp(prefix="ui-smoke-"))
    os.environ["XDG_STATE_HOME"] = str(tmp/"state")
    os.environ["XDG_RUNTIME_DIR"] = str(tmp/"run")
    os.environ["XDG_CONFIG_HOME"] = str(tmp/"config")
    os.environ["XDG_DATA_HOME"] = str(tmp/"data")
    (tmp/"run").mkdir(parents=True, exist_ok=True)
    m = SourceFileLoader("slopbox", "/home/user/sarun/sarun").load_module()
    m.ensure_dirs()
    try:
        asyncio.run(drive(m))
    except Exception as e:
        import traceback; traceback.print_exc(); _fails.append(str(e))
    finally:
        # safety: lazy-unmount the temp mnt if anything is still attached
        mnt = tmp/"run"/"slopbox"/"mnt"
        try:
            if mnt.exists() and os.path.ismount(str(mnt)):
                subprocess.run(["fusermount3","-uz",str(mnt)],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                               timeout=10)
        except Exception: pass
        shutil.rmtree(tmp, ignore_errors=True)
    print("\n" + ("UI SMOKE PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


if __name__ == "__main__":
    sys.exit(main())
