#!/usr/bin/env python3
"""fsx data-integrity battery against the live FUSE overlay.

fsx (the classic file-system exerciser) drives a long randomized stream of
read / write / mmap-read / mmap-write / truncate ops on one file, keeping an
in-memory model of the correct contents and aborting the instant a read returns
the wrong bytes. Run inside the overlay it exercises the capture-write path —
lazy handle, RAM buffer, pool-blob spill, copy-up, truncate — for data
corruption, which the metadata-only pjdfstest can't see.

Note: fsx opens the file O_RDWR, so it does NOT exercise the read-only
keep_cache path (that's what test the bespoke coherence check covers); this is
the integrity net for the write side.

    /home/user/venv/bin/python test_fsx.py        # or: pytest test_fsx.py
"""
import os
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent / "bench"))
import extsuite

_fails = []


def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond:
        _fails.append(msg)


def _run_fsx(box_root, fsx, numops, extra):
    workdir = os.path.join(box_root, "root", "fsxwd")
    os.makedirs(workdir, exist_ok=True)
    # -N: number of ops. Deterministic seed (-S) so a failure is reproducible.
    cmd = [str(fsx), "-N", str(numops), "-S", "1", *extra, "fsxfile"]
    p = subprocess.run(cmd, cwd=workdir, capture_output=True, text=True)
    tail = "\n".join(p.stdout.splitlines()[-3:])
    print(f"    fsx {' '.join(extra) or '(default)'}: rc={p.returncode}  {tail}")
    if p.returncode != 0:
        print(p.stdout[-1500:]); print(p.stderr[-1500:])
    return p.returncode == 0


def test_fsx_integrity():
    if not extsuite.fuse_available():
        return extsuite.skip("no /dev/fuse or fusermount3")
    fsx = extsuite.ensure_fsx()
    with extsuite.overlay_session() as box_root:
        # Default mix (incl. mmap), then a no-mmap pass to stress the plain
        # read/write/truncate path harder.
        check(_run_fsx(box_root, fsx, 20000, []),
              "fsx 20000 ops (read/write/mmap/truncate) — no corruption")
        check(_run_fsx(box_root, fsx, 20000, ["-R", "-W"]),
              "fsx 20000 ops (no mmap) — no corruption")


if __name__ == "__main__":
    try:
        test_fsx_integrity()
    except extsuite._Skip:
        sys.exit(0)
    except Exception:
        import traceback
        traceback.print_exc()
        _fails.append("exception")
    print("\n" + ("ALL PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    sys.exit(1 if _fails else 0)
