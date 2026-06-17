#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["pyfuse3>=3.2","trio>=0.22","python-magic>=0.4","wcmatch>=8.4"]
# ///
"""perop_overlay — isolate the SERIAL per-op overhead of the full Rust overlay
vs the Python overlay (single cold git-status walk; one in-flight request, so
threading is NOT a factor). The companion to parallel_build_rs.py: that one
measures the threading axis, this one the language/per-op axis."""
import os, socket, subprocess, tempfile, shutil, time, sys
from pathlib import Path
from importlib.machinery import SourceFileLoader
sys.path.insert(0, str(Path(__file__).resolve().parent))
import overlay_bench as ob

BIN = "/home/user/sarun/engine/target/release/sarun-engine"
DIRS, PERDIR = 100, 50
OPS = DIRS * (PERDIR + 2)   # ~1 lstat/file + readdirs, same as workloads.py
GIT = ["git", "status", "--porcelain"]
REL = "root/peroprepo"

def make_repo():
    p = Path("/root/peroprepo"); shutil.rmtree(p, ignore_errors=True); p.mkdir()
    env=dict(os.environ,GIT_AUTHOR_NAME="b",GIT_AUTHOR_EMAIL="b@b",
             GIT_COMMITTER_NAME="b",GIT_COMMITTER_EMAIL="b@b")
    subprocess.run(["git","init","-q","."],cwd=p,check=True,env=env)
    for d in range(DIRS):
        dd=p/f"d{d}";dd.mkdir()
        for f in range(PERDIR): (dd/f"f{f}").write_text(f"{d} {f}\n")
    subprocess.run(["git","add","-A"],cwd=p,check=True,env=env)
    subprocess.run(["git","-c","commit.gpgsign=false","commit","-qm","s"],cwd=p,check=True,env=env)

def gitwalk(box_root, overlay):
    # cold: first run after a fresh mount/box. one serial walk.
    t0=time.monotonic()
    p=subprocess.run(ob.make_bwrap(box_root, f"/{REL}", GIT, overlay=overlay),
                     stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    assert p.returncode==0, p.stderr.decode()[-300:]
    return time.monotonic()-t0

make_repo()
# native warm reference (workloads.py methodology)
gitwalk(None, False)
nat=min(gitwalk(None, False) for _ in range(5))

# Rust full overlay: fresh box each trial = cold inodes
def rust_cold():
    tmp=Path(tempfile.mkdtemp())
    for k,s in (('XDG_STATE_HOME','state'),('XDG_RUNTIME_DIR','run'),('XDG_CONFIG_HOME','c'),('XDG_DATA_HOME','d')): os.environ[k]=str(tmp/s)
    os.environ['SLOPBOX_NS']='PO';(tmp/'run').mkdir(parents=True)
    m=SourceFileLoader('slopbox','/home/user/sarun/prototype/sarun').load_module();m.ensure_dirs()
    eng=subprocess.Popen([BIN,'serve'],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    sock=m.sock_path()
    for _ in range(150):
      try: s=socket.socket(socket.AF_UNIX);s.connect(sock);s.close();break
      except OSError: time.sleep(0.1)
    r=m.sync_request(sock,type='ui',verb='box_new',args=[])
    dt=gitwalk(r['r']['root'], True)
    eng.terminate();eng.wait();shutil.rmtree(tmp,ignore_errors=True)
    return dt
rust=min(rust_cold() for _ in range(3))
del os.environ['SLOPBOX_NS']

# Python full overlay: fresh session each = cold
def py_cold():
    tmp=Path(tempfile.mkdtemp())
    for k,s in (('XDG_STATE_HOME','state'),('XDG_RUNTIME_DIR','run'),('XDG_CONFIG_HOME','c'),('XDG_DATA_HOME','d')): os.environ[k]=str(tmp/s)
    (tmp/'run').mkdir(parents=True)
    import importlib;importlib.reload(ob);mm=ob.load_sarun()
    tr,mount,broot,sid=ob.setup_overlay(mm)
    dt=gitwalk(broot, True)
    try: mount.stop()
    except: pass
    shutil.rmtree(tr,ignore_errors=True);shutil.rmtree(tmp,ignore_errors=True)
    return dt
py=min(py_cold() for _ in range(3))

shutil.rmtree("/root/peroprepo", ignore_errors=True)
print(f"\ncold git-status, single serial walk, ~{OPS} metadata ops:")
print(f"  native  {nat*1000:6.1f} ms")
print(f"  python  {py*1000:6.1f} ms   per-op overhead {(py-nat)/OPS*1e6:5.1f} us/op")
print(f"  rust    {rust*1000:6.1f} ms   per-op overhead {(rust-nat)/OPS*1e6:5.1f} us/op")
