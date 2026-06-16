#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["pyfuse3>=3.2", "trio>=0.22", "python-magic>=0.4", "wcmatch>=8.4"]
# ///
"""parallel_metadata — K concurrent cold git-status walks through: native, the
Rust m1 passthrough engine (engine/, 1 vs N threads), and the Python overlay.
The engine-rewrite scaling proof; see FINDINGS.md. Build the engine first:
    cargo build --release --manifest-path engine/Cargo.toml"""
import os, shutil, subprocess, sys, time
from pathlib import Path
sys.path.insert(0, "/home/user/sarun/bench")
import overlay_bench as ob

BASE = Path("/root/parmeta")
K = 4
ENG = "/home/user/sarun/engine/target/release/sarun-engine"

def make_repos():
    if BASE.exists(): shutil.rmtree(BASE)
    BASE.mkdir()
    src = BASE / "r0"; src.mkdir()
    env = dict(os.environ, GIT_AUTHOR_NAME="b", GIT_AUTHOR_EMAIL="b@b",
               GIT_COMMITTER_NAME="b", GIT_COMMITTER_EMAIL="b@b")
    subprocess.run(["git","init","-q","."], cwd=src, check=True, env=env)
    for d in range(100):
        dd = src/f"dir{d}"; dd.mkdir()
        for f in range(50): (dd/f"f{f}.txt").write_text(f"c {d} {f}\n")
    subprocess.run(["git","add","-A"], cwd=src, check=True, env=env)
    subprocess.run(["git","-c","commit.gpgsign=false","commit","-qm","s"],
                   cwd=src, check=True, env=env)
    for k in range(1, K): shutil.copytree(src, BASE/f"r{k}")

def concurrent_status(prefix):
    procs = [subprocess.Popen(
        ["git","status","--porcelain"], cwd=f"{prefix}/root/parmeta/r{k}",
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL) for k in range(K)]
    t0 = time.monotonic()
    rcs = [p.wait() for p in procs]
    return time.monotonic() - t0, rcs

make_repos()
# native (warm reference)
concurrent_status("")  # warm the page/dentry caches
nat, _ = concurrent_status("")
print(f"native     {K} concurrent git status: {nat:6.3f}s  (warm reference)")

# rust m1, 1 thread vs 8 threads — fresh mount each => cold caches
for thr in (1, 8):
    mnt = Path("/root/rsmnt"); mnt.mkdir(exist_ok=True)
    e = subprocess.Popen([ENG, str(mnt), "--lower", "/", "--threads", str(thr)],
                         stderr=subprocess.DEVNULL)
    time.sleep(1)
    dt, rcs = concurrent_status(str(mnt))
    print(f"rust({thr}t)   {K} concurrent cold git status: {dt:6.3f}s  rcs={rcs}")
    subprocess.run(["fusermount3","-u",str(mnt)]); e.wait()

# python overlay (single trio thread) — fresh session = cold
sarun = ob.load_sarun()
tmproot, mount, box_root, sid = ob.setup_overlay(sarun)
try:
    dt, rcs = concurrent_status(box_root)
    print(f"python     {K} concurrent cold git status: {dt:6.3f}s  rcs={rcs}")
finally:
    try: mount.stop()
    except Exception: pass
    shutil.rmtree(tmproot, ignore_errors=True)
shutil.rmtree(BASE, ignore_errors=True)
