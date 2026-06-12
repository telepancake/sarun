#!/usr/bin/env python3
"""Attach-mode parity: the SAME Textual UI runs as a pure CLIENT of a real engine
subprocess — every pane reads over the control socket; no mount, no server in the
UI process. Drives the remote app with app.run_test() against an engine that
discovered a real finished box on disk, and checks the panes show the box's data
(sessions list, changes, procs, outputs) end-to-end through the RPC layer. Run:
    uv run --with "textual>=0.60" --with "pyfuse3>=3.2" --with "trio>=0.22" \
      --with "wcmatch>=8.4" --with "python-magic>=0.4" python test_attach_remote.py
Self-safety: XDG dirs redirected to a temp tree; the engine subprocess is killed
in finally; its mount lives under the temp runtime dir.
"""
import os, sys, asyncio, json, socket, subprocess, tempfile, shutil, time
import stat as stat_mod
from pathlib import Path
from importlib.machinery import SourceFileLoader

SARUN = "/home/user/sarun/sarun"

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


def wait_socket(sock, timeout=60):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.settimeout(1.0); s.connect(sock); return True
        except OSError:
            time.sleep(0.2)
    return False


async def drive(m, sid):
    app = m._make_ui_app()(remote=True)
    async with app.run_test() as pilot:
        await pilot.pause()
        check(app.remote and app.overlay_mount is None,
              "attach: remote app holds no local mount")
        check(type(app.sup).__name__ == "RemoteSupervisor",
              "attach: sup is the RPC facade")
        # engine's discovered box arrived via session_dicts over the socket
        check(sid in app.sessions, "attach: finished box listed over the wire")
        app._select_sid(sid); await pilot.pause()
        # changes pane: entries come from review.session_changes RPC
        await pilot.press("c"); await pilot.pause(0.3); await pilot.pause()
        rels = {e.get("path") for e in app._ch_entries}
        check("afile.txt" in rels, "attach: changes pane shows the captured file")
        # procs pane: process rows over the wire
        await pilot.press("p"); await pilot.pause(0.3); await pilot.pause()
        check(len(app._pr_procs) >= 1, "attach: procs pane has the writer row")
        # outputs pane renders (box has no outputs — just must not error)
        await pilot.press("o"); await pilot.pause()
        check(app.view == "outputs", "attach: outputs view renders")
        await pilot.press("b"); await pilot.pause()
    # quitting an ATTACHED (not auto-spawned) UI must leave the engine running
    check(wait_socket(os.environ["_ENG_SOCK"], 5),
          "attach: engine still answers after the attached UI quit")


def main():
    tmp = Path(tempfile.mkdtemp(prefix="attach-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    (tmp / "run").mkdir(parents=True, exist_ok=True)
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    eng = None
    try:
        # a real finished box on disk for the engine to discover
        sid = "4001"
        backing = m.live_dir(sid); (backing / "up").mkdir(parents=True)
        idx = m.Index(backing)
        wid = idx.writer_for(os.getpid())
        idx.set_entry("afile.txt", "file", stat_mod.S_IFREG | 0o644, wid, "create")
        bp = m.blob_path(idx.box_id, idx.row_id("afile.txt"))
        bp.parent.mkdir(parents=True, exist_ok=True)
        bp.write_bytes(b"hello attach\n")
        m.consolidate(str(backing), sid, index=idx)
        idx.close()

        eng = subprocess.Popen([sys.executable, SARUN, "engine"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        os.environ["_ENG_SOCK"] = m.sock_path()
        if not wait_socket(m.sock_path(), 60):
            out = eng.stdout.read(4000) if eng.stdout else b""
            raise RuntimeError("engine socket never appeared:\n"
                               + out.decode(errors="replace"))
        asyncio.run(drive(m, sid))
    except Exception as e:
        import traceback; traceback.print_exc(); _fails.append(str(e))
    finally:
        if eng is not None:
            eng.terminate()
            try: eng.wait(timeout=15)
            except Exception: eng.kill()
        shutil.rmtree(tmp, ignore_errors=True)
    print("\n" + ("ATTACH PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_attach_remote():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
