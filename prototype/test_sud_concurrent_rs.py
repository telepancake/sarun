#!/usr/bin/env python3
"""Concurrent filesystem-semantics validation for the sud backend.

Until now NOTHING validated `--sud` overlay/inramfs semantics under PARALLEL
access — the equivalence test is single-threaded, the C overlay tests are
single-threaded, and the one "concurrent" test is FUSE-only single-file
read+write. But the real workload — `make -jN` under `-b` — runs recipe threads
IN ONE PROCESS, all taking SIGSYS concurrently and hammering the SAME overlay
state (the per-process synth merged-directory machinery, the fd/dirfd maps, the
shared inramfs /tmp). This test drives that model with CONTENTION on a shared
directory and asserts the invariants a filesystem must keep:

  - a file written then read back has exactly the bytes written (no torn /
    stale / empty reads under a racing copy-up or synth rebuild),
  - every file created before a readdir is listed (no "No such file" from a
    stale merged-directory snapshot),
  - the final count matches (no lost or duplicated entries),
  - `mv tmp final` (atomic-write pattern) lands and is visible.

Two workloads, both must report ALLOK: brush background jobs (`&`) contending
on one directory, and `make -j` contending on one output directory with a
gather step that must see every file.

Run:
    uv run --with "wcmatch>=8.4" --with "python-magic>=0.4" \
      python test_sud_concurrent_rs.py
Skips (passes vacuously) if the binary / sud64 / bwrap are unavailable.
"""
import os, shutil, socket, subprocess, sys, tempfile, time
from pathlib import Path
from importlib.machinery import SourceFileLoader

_HERE = Path(__file__).resolve().parent
SARUN = str(_HERE / "libtestsarun.py")
CRATE = _HERE.parent / "engine"
BIN = CRATE / "target/x86_64-unknown-linux-musl/release/sarun"
TV = _HERE.parent / "tv"
SUD64 = TV / "sud64"
TMPBASE = "/var/tmp"   # box /tmp is inramfs; keep engine state off /tmp

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond:
        _fails.append(msg)


def wait_socket(sock, timeout=10.0):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.settimeout(1.0); s.connect(sock); return True
        except OSError:
            time.sleep(0.1)
    return False


# --- workloads (run inside the box, self-checking) ------------------------

# N background workers each create M files (via tmp+rename) in ONE shared dir,
# reading each back immediately; then a barrier, a readdir count, and a full
# read-back. Prints ALLOK or the specific broken invariant.
BG_WORKLOAD = r'''
set -u
cd /root/w
mkdir -p shared
N=24; M=40
: > errors
i=1
while [ $i -le $N ]; do
  (
    j=1
    while [ $j -le $M ]; do
      f=shared/w${i}_${j}
      echo "c-$i-$j" > "$f.tmp"
      mv "$f.tmp" "$f"
      got=$(cat "$f" 2>/dev/null)
      [ "$got" = "c-$i-$j" ] || echo "MISMATCH $f got=[$got]" >> errors
      j=$((j+1))
    done
  ) &
  i=$((i+1))
done
wait
n=$(ls shared | wc -l)
[ "$n" = "$((N*M))" ] || echo "COUNT $n != $((N*M))" >> errors
for f in shared/*; do cat "$f" >/dev/null 2>>errors || echo "READFAIL $f" >> errors; done
if [ -s errors ]; then echo "BG-ERRORS"; head -8 errors; else echo BG-ALLOK; fi
'''

MAKE_WORKLOAD = r'''
set -u
cd /root/w
cat > Makefile <<'MK'
N := $(shell seq 1 120)
OUT := $(addprefix shared/f,$(N))
all: gather
shared/f%:
	@mkdir -p shared
	@echo "val-$*" > shared/f$*.tmp
	@mv shared/f$*.tmp shared/f$*
	@ls shared >/dev/null
gather: $(OUT)
	@cnt=$$(ls shared | wc -l); [ "$$cnt" = "120" ] || { echo "MK-COUNT $$cnt"; exit 1; }
	@for f in $(OUT); do test -r $$f || { echo "MK-MISSING $$f"; exit 1; }; done
	@echo MK-ALLOK
MK
make -j32
'''


class Box:
    def __init__(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="sudconc-", dir=TMPBASE))
        for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                       ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
            os.environ[k] = str(self.tmp / sub)
        os.environ["SLOPBOX_NS"] = "SUDCONC"
        os.environ["SARUN_SUD64"] = str(SUD64)
        self.m = SourceFileLoader("slopbox", SARUN).load_module()
        self.m.ensure_dirs()
        self.proc = subprocess.Popen([str(BIN), "serve"],
                                     stdout=subprocess.DEVNULL,
                                     stderr=subprocess.DEVNULL)
        if not wait_socket(self.m.sock_path()):
            raise RuntimeError("engine socket never appeared")

    def run(self, name, script):
        return subprocess.run(
            [str(BIN), "run", name, "--sud", "--net", "off", "--",
             "bash", "-c", script],
            capture_output=True, text=True, timeout=180)

    def close(self):
        try:
            self.proc.terminate(); self.proc.wait(timeout=10)
        except Exception:
            self.proc.kill()


def main():
    if not BIN.exists() or not SUD64.exists() or shutil.which("bwrap") is None:
        print("skip: binary / sud64 / bwrap unavailable")
        return 0
    # Fresh work tree on the real fs (a lower layer the box overlays).
    work = Path("/root/w")
    box = Box()
    try:
        # A few rounds — a race that fires 1-in-K only shows with repetition.
        for rnd in range(4):
            if work.exists():
                shutil.rmtree(work, ignore_errors=True)
            work.mkdir(parents=True, exist_ok=True)
            r = box.run(f"BG{rnd}", BG_WORKLOAD)
            check("BG-ALLOK" in r.stdout,
                  f"round {rnd}: background-job contention on a shared dir "
                  f"holds fs invariants (out={r.stdout.strip()[-200:]!r})")

            shutil.rmtree(work, ignore_errors=True); work.mkdir(parents=True)
            r = box.run(f"MK{rnd}", MAKE_WORKLOAD)
            check("MK-ALLOK" in r.stdout,
                  f"round {rnd}: make -j32 contention on a shared output dir "
                  f"holds fs invariants (out={r.stdout.strip()[-200:]!r})")
    finally:
        box.close()
        shutil.rmtree(work, ignore_errors=True)
    print("\n" + ("SUD-CONCURRENT PASS" if not _fails
                  else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_sud_concurrent():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
