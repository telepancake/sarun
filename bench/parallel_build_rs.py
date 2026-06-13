#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["pyfuse3>=3.2","trio>=0.22","python-magic>=0.4","wcmatch>=8.4"]
# ///
"""parallel_build_rs — the graduation benchmark: make -jN of a C project through
native / the Python overlay / the Rust engine overlay (engine/), proving the
Rust multithreaded serving loop breaks the GIL ceiling under load. Build the
engine first: cargo build --release --manifest-path engine/Cargo.toml"""
import os, socket, subprocess, tempfile, shutil, time
from pathlib import Path
from importlib.machinery import SourceFileLoader
import sys
sys.path.insert(0, str(Path(__file__).resolve().parent))
import overlay_bench as ob

BIN = "/home/user/sarun/engine/target/release/sarun-engine"
N = 200

def make_proj(d):
    d.mkdir(parents=True)
    for i in range(N):
        (d/f"f{i}.c").write_text(f"int fn{i}(int x){{return x*{i+1};}}\n")
    (d/"Makefile").write_text(
        "SRCS=$(wildcard *.c)\nOBJS=$(SRCS:.c=.o)\nall: $(OBJS)\n"
        "%.o: %.c\n\tcc -O0 -c $< -o $@\nclean:\n\trm -f *.o\n")

def build(box_root, proj_rel, jobs, overlay):
    sub = f"/{proj_rel}"
    subprocess.run(ob.make_bwrap(box_root, sub, ["make","clean"], overlay=overlay),
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    t0=time.monotonic()
    p=subprocess.run(ob.make_bwrap(box_root, sub, ["make",f"-j{jobs}"], overlay=overlay),
                     stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    assert p.returncode==0, p.stderr.decode()[-400:]
    return time.monotonic()-t0

# project on the host
proj = Path("/root/gradproj"); shutil.rmtree(proj, ignore_errors=True); make_proj(proj)
rel = "root/gradproj"

# --- Rust engine: register a box via box_new, build through <mnt>/<id> ---
tmp=Path(tempfile.mkdtemp())
for k,s in (('XDG_STATE_HOME','state'),('XDG_RUNTIME_DIR','run'),('XDG_CONFIG_HOME','c'),('XDG_DATA_HOME','d')): os.environ[k]=str(tmp/s)
os.environ['SLOPBOX_NS']='GRAD';(tmp/'run').mkdir(parents=True)
m=SourceFileLoader('slopbox','/home/user/sarun/sarun').load_module(); m.ensure_dirs()
eng=subprocess.Popen([BIN,'serve'],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
sock=m.sock_path()
for _ in range(150):
  try: s=socket.socket(socket.AF_UNIX);s.connect(sock);s.close();break
  except OSError: time.sleep(0.1)
r=m.sync_request(sock,type='ui',verb='box_new',args=[]);rbox=r['r']['root']
try:
    rust1=min(build(rbox, rel, 1, True) for _ in range(2))
    rust8=min(build(rbox, rel, 8, True) for _ in range(2))
finally:
    eng.terminate();eng.wait();shutil.rmtree(tmp,ignore_errors=True)
del os.environ['SLOPBOX_NS']

# --- Python engine overlay (single trio thread) ---
tmp2=Path(tempfile.mkdtemp())
for k,s in (('XDG_STATE_HOME','state'),('XDG_RUNTIME_DIR','run'),('XDG_CONFIG_HOME','c'),('XDG_DATA_HOME','d')): os.environ[k]=str(tmp2/s)
(tmp2/'run').mkdir(parents=True)
import importlib;importlib.reload(ob);m2=ob.load_sarun()
tr,mount,broot,sid=ob.setup_overlay(m2)
try:
    py1=min(build(broot, rel, 1, True) for _ in range(2))
    py8=min(build(broot, rel, 8, True) for _ in range(2))
finally:
    try: mount.stop()
    except: pass
    shutil.rmtree(tr,ignore_errors=True);shutil.rmtree(tmp2,ignore_errors=True)

# --- native baseline ---
nat1=min(build(None, rel, 1, False) for _ in range(2))
nat8=min(build(None, rel, 8, False) for _ in range(2))
shutil.rmtree(proj, ignore_errors=True)
print(f"\nmake build of {N} files (best of 2):")
print(f"{'':10}{'-j1':>9}{'-j8':>9}{'scaling':>9}")
for lbl,t1,t8 in (("native",nat1,nat8),("python",py1,py8),("rust",rust1,rust8)):
    print(f"{lbl:10}{t1:>8.2f}s{t8:>8.2f}s{t1/t8:>8.1f}x")
print(f"\noverlay/native at -j8:  python {py8/nat8:.1f}x   rust {rust8/nat8:.1f}x")
