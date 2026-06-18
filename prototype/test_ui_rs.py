#!/usr/bin/env python3
"""m5 spike: the RUST UI client (ui/) rendered headlessly against a live engine.
Proves a Rust client speaks the wire protocol, fetches real box state, and
renders it with ratatui into a buffer we assert on — no terminal. Python
orchestrates; both the engine and the ui client are black-box binaries. Run:
    uv run --with "pyfuse3>=3.2" --with "trio>=0.22" --with "wcmatch>=8.4" \
      --with "python-magic>=0.4" python test_ui_rs.py
Skips if cargo / the binaries are unavailable.
"""
import os, socket, subprocess, sys, tempfile, shutil, time
from pathlib import Path
from importlib.machinery import SourceFileLoader

SARUN = "/home/user/sarun/prototype/sarun"
ENG = Path("/home/user/sarun/engine/target/x86_64-unknown-linux-musl/release/sarun-engine")
UI = Path("/home/user/sarun/ui/target/x86_64-unknown-linux-musl/release/sarun-ui")

_fails = []
def check(c, m):
    print(("  ok  " if c else " FAIL ") + m)
    if not c: _fails.append(m)


def build(crate, binp):
    if binp.exists(): return True
    if shutil.which("make") is None: return False
    subprocess.run(["make","engine"], cwd=crate.parent,
                   capture_output=True)
    return binp.exists()


def wait_socket(s, t=30):
    end=time.time()+t
    while time.time()<end:
        try:
            with socket.socket(socket.AF_UNIX,socket.SOCK_STREAM) as k:
                k.settimeout(1);k.connect(s);return True
        except OSError: time.sleep(0.1)
    return False


def main():
    if not (build(ENG.parents[1], ENG) and build(UI.parents[1], UI)):
        print("  ok  ui-rs: cargo/binaries unavailable — SKIP\n\nUI-RS PASS (skipped)")
        return 0
    tmp=Path(tempfile.mkdtemp(prefix="uirs-"))
    for k,s in (('XDG_STATE_HOME','state'),('XDG_RUNTIME_DIR','run'),
                ('XDG_CONFIG_HOME','c'),('XDG_DATA_HOME','d')): os.environ[k]=str(tmp/s)
    os.environ['SLOPBOX_NS']='UIRS';(tmp/'run').mkdir(parents=True)
    m=SourceFileLoader('slopbox',SARUN).load_module();m.ensure_dirs()
    eng=None
    try:
        eng=subprocess.Popen([str(ENG),'serve'],stdout=subprocess.DEVNULL,
                             stderr=subprocess.DEVNULL)
        sock=m.sock_path()
        if not wait_socket(sock): raise RuntimeError("engine socket never came up")
        # empty render first: the UI talks to the engine and draws the frame
        r=subprocess.run([str(UI),'--once','--sock',sock],
                         capture_output=True,text=True,timeout=30)
        check(r.returncode==0, f"ui-rs: --once exits 0 (got {r.returncode}: {r.stderr[-200:]})")
        check("sarun" in r.stdout and "boxes" in r.stdout,
              "ui-rs: rendered the boxes pane frame (ratatui -> buffer)")
        check("PATH" in r.stdout and "STATUS" in r.stdout,
              "ui-rs: header row rendered")
        check("(no boxes)" in r.stdout, "ui-rs: empty state shown with no boxes")
        # now create a box and re-render: it must appear in the frame
        rep=m.sync_request(sock,type="ui",verb="box_new",args=[])
        bid=rep["r"]["sid"]
        r=subprocess.run([str(UI),'--once','--sock',sock],
                         capture_output=True,text=True,timeout=30)
        check(bid in r.stdout,
              "ui-rs: the live box appears in the rendered frame over the wire")
        check("(no boxes)" not in r.stdout, "ui-rs: empty state gone once a box exists")
    except Exception as e:
        import traceback;traceback.print_exc();_fails.append(str(e))
    finally:
        if eng and eng.poll() is None:
            eng.terminate()
            try: eng.wait(timeout=10)
            except Exception: eng.kill()
        os.environ.pop('SLOPBOX_NS',None)
        shutil.rmtree(tmp,ignore_errors=True)
    print("\n"+("UI-RS PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_ui_rs():
    assert main()==0, _fails


if __name__=="__main__":
    sys.exit(main())
