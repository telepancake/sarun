#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["pyfuse3>=3.2", "trio>=0.22", "python-magic>=0.4", "wcmatch>=8.4"]
# ///
"""parallel_build — make -jN scaling through the overlay vs native: measures the
GIL/single-thread serving ceiling under a parallel compile (the workload serial
benchmarks understate). Same bwrap both sides; see FINDINGS.md addendum."""
import os, shutil, subprocess, sys, tempfile, time
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent))
import overlay_bench as ob

work = Path(tempfile.mkdtemp(prefix="parbench-", dir="/root"))
src = work / "proj"; src.mkdir()
N = 200
for i in range(N):
    (src / f"f{i}.c").write_text(
        f"#include <stdio.h>\n#include <stdlib.h>\n#include <string.h>\n"
        f"int fn{i}(int x){{ char b[64]; snprintf(b,64,\"%d\",x*{i+1}); "
        f"return (int)strlen(b)+x; }}\n")
(src / "Makefile").write_text(
    "SRCS=$(wildcard *.c)\nOBJS=$(SRCS:.c=.o)\nall: $(OBJS)\n"
    "%.o: %.c\n\tcc -O1 -c $< -o $@\nclean:\n\trm -f *.o\n")

def run(box_root, jobs, overlay):
    subprocess.run(ob.make_bwrap(box_root, str(src), ["make","clean"], overlay=overlay),
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    t0=time.monotonic()
    p=subprocess.run(ob.make_bwrap(box_root, str(src), ["make",f"-j{jobs}"], overlay=overlay),
                     stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    assert p.returncode==0, p.stderr.decode()[-500:]
    return time.monotonic()-t0

sarun = ob.load_sarun()
tmproot, mount, box_root, sid = ob.setup_overlay(sarun)
try:
    # warmup (page/dentry caches, cc1 priming) then best-of-2 each
    run(None, 8, False); run(box_root, 8, True)
    res={}
    for label, br, ov in (("native", None, False), ("overlay", box_root, True)):
        for j in (1, 8):
            res[(label,j)] = min(run(br,j,ov) for _ in range(2))
    print(f"{'':<9}{'-j1':>8}{'-j8':>8}{'scaling':>9}")
    for label in ("native","overlay"):
        t1,t8 = res[(label,1)], res[(label,8)]
        print(f"{label:<9}{t1:>7.2f}s{t8:>7.2f}s{t1/t8:>8.1f}x")
    print(f"overlay/native: -j1 {res[('overlay',1)]/res[('native',1)]:.2f}x"
          f"   -j8 {res[('overlay',8)]/res[('native',8)]:.2f}x")
finally:
    try: mount.stop()
    except Exception: pass
    shutil.rmtree(tmproot, ignore_errors=True)
    shutil.rmtree(work, ignore_errors=True)
