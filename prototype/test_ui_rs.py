#!/usr/bin/env python3
"""The Rust UI rendered headlessly against a live engine. The UI lives in the
engine binary (engine/src/ui.rs); `sarun --once --sock PATH` renders one frame
to a ratatui TestBackend, prints it, and exits. Proves the client speaks the
wire protocol, fetches real box state, and renders it — no terminal. Run:
    uv run --with "pyfuse3>=3.2" --with "trio>=0.22" --with "wcmatch>=8.4" \
      --with "python-magic>=0.4" python test_ui_rs.py
"""
import os, socket, subprocess, sys, tempfile, shutil, time
from pathlib import Path
from importlib.machinery import SourceFileLoader

SARUN = "/home/user/sarun/prototype/sarun"
ENG = Path("/home/user/sarun/engine/target/x86_64-unknown-linux-musl/release/sarun")

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
    assert build(ENG.parents[1], ENG), f"engine binary missing: {ENG} — run `make engine`"
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
        r=subprocess.run([str(ENG),'--once','--sock',sock],
                         capture_output=True,text=True,timeout=30)
        check(r.returncode==0, f"ui-rs: --once exits 0 (got {r.returncode}: {r.stderr[-200:]})")
        check("sarun" in r.stdout and "boxes" in r.stdout,
              "ui-rs: rendered the boxes pane frame (ratatui -> buffer)")
        check("Name" in r.stdout and "PID" in r.stdout and "Cmd" in r.stdout,
              "ui-rs: header row rendered")
        check("(no boxes)" in r.stdout, "ui-rs: empty state shown with no boxes")
        # now create a box and re-render: it must appear in the frame
        rep=m.sync_request(sock,type="ui",verb="box_new",args=[])
        bid=rep["r"]["sid"]
        r=subprocess.run([str(ENG),'--once','--sock',sock],
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
