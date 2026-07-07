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


# Single-process POSIX fs-semantics matrix (run inside the box, self-checking).
# These are the dimensions a userland overlay re-implements and can get wrong:
# truncate grow/shrink, sparse holes, mtime ordering, mode bits, rename-over
# atomicity, UNLINKED-FILE-HELD-BY-FD (read+write survive the unlink), O_APPEND,
# readdir-sees-create, mid-file seek/overwrite. Prints one PASS/FAIL per probe.
SEMANTICS_WORKLOAD = r'''
set -u
cd /root/w
P(){ echo "PASS $1"; }
F(){ echo "FAIL $1 :: $2"; }
printf 'abcdef' > t; truncate -s 3 t
[ "$(stat -c%s t)" = 3 ] && [ "$(cat t)" = abc ] && P trunc-shrink || F trunc-shrink "$(stat -c%s t)/$(cat t)"
truncate -s 6 t; [ "$(stat -c%s t)" = 6 ] && P trunc-grow || F trunc-grow "$(stat -c%s t)"
rm -f sp; dd if=/dev/zero of=sp bs=1 seek=1048576 count=1 2>/dev/null
[ "$(stat -c%s sp)" = 1048577 ] && P sparse-size || F sparse-size "$(stat -c%s sp)"
[ "$(stat -c%b sp)" -lt 200 ] 2>/dev/null && P sparse-holes || F sparse-holes "blocks=$(stat -c%b sp)"
echo a > m; m1=$(stat -c%Y m); sleep 1.1; echo b > m; m2=$(stat -c%Y m)
[ "$m2" -gt "$m1" ] && P mtime-advances || F mtime-advances "$m1/$m2"
echo x > c; chmod 741 c; [ "$(stat -c%a c)" = 741 ] && P chmod-bits || F chmod-bits "$(stat -c%a c)"
echo AAA > rx; echo BBB > ry; mv ry rx
[ "$(cat rx)" = BBB ] && [ ! -e ry ] && P rename-over || F rename-over "$(cat rx)/$([ -e ry ]&&echo y)"
( exec 7>uf; echo LIVE >&7; rm uf; [ -e uf ] && { echo "FAIL unlink-name-gone :: listed"; exit; }
  echo MORE >&7; exec 7>&-; echo "PASS unlink-fd-write" )
printf HELD > uf2; exec 8<uf2; rm uf2; r=$(cat <&8); exec 8<&-
[ "$r" = HELD ] && P unlink-fd-read || F unlink-fd-read "[$r]"
rm -f ap; echo one >> ap; echo two >> ap; [ "$(wc -l < ap)" = 2 ] && P append || F append "$(wc -l < ap)"
mkdir -p d9; echo z > d9/file; ls d9 | grep -q '^file$' && P readdir-sees-create || F readdir-sees-create "[$(ls d9)]"
printf '0123456789' > sk; printf XY | dd of=sk bs=1 seek=4 conv=notrunc 2>/dev/null
[ "$(cat sk)" = '0123XY6789' ] && P seek-overwrite || F seek-overwrite "$(cat sk)"
echo SEMANTICS-DONE
'''

# Concurrent VISIBILITY under contention: a writer creates 300 STABLE files
# (temp lives outside the read dir, so readers never glob a mid-rename temp),
# while 6 readers repeatedly list the dir and read every file. A stable file
# that is listed but not readable — or reads empty — is a merged-directory /
# copy-up visibility bug. Ends with an exact-count + all-readable check.
VIS_WORKLOAD = r'''
set -u
cd /root/w
: > errors; mkdir -p x tmp
( for i in $(seq 1 300); do echo "v$i" > tmp/$i; mv tmp/$i x/f$i; done; : > x/.done ) &
for r in 1 2 3 4 5 6; do
 ( while [ ! -e x/.done ]; do
     for f in x/f*; do
       [ "$f" = 'x/f*' ] && continue
       if ! v=$(cat "$f" 2>/dev/null); then echo "READFAIL $f" >> errors
       elif [ -z "$v" ]; then echo "EMPTYREAD $f" >> errors; fi
     done
   done ) &
done
wait
for i in $(seq 1 300); do [ -r x/f$i ] || echo "FINAL-MISSING x/f$i" >> errors; done
cnt=$(ls x | grep -c '^f'); [ "$cnt" = 300 ] || echo "FINAL-COUNT $cnt" >> errors
if [ -s errors ]; then echo "VIS-ERRORS"; sort errors | uniq -c | head; else echo VIS-ALLOK; fi
'''

# The box's /tmp is a SEPARATE hand-rolled in-memory fs (inramfs), and the rest
# is the copy-on-write overlay — different code, different bugs. This exercises
# inramfs semantics directly AND the overlay<->inramfs boundary (rename/cp both
# directions), plus inramfs under 6-reader concurrency. Builds live in /tmp
# (TMPDIR / configure / compilers), so this path is not optional.
INRAMFS_WORKLOAD = r'''
set -u
P(){ echo "PASS $1"; }
F(){ echo "FAIL $1 :: $2"; }
D=/tmp/ir; rm -rf $D; mkdir -p $D; cd $D
printf 'abcdef' > t; truncate -s 3 t; [ "$(cat t)" = abc ] && P ir-trunc || F ir-trunc "$(cat t)"
echo AAA > rx; echo BBB > ry; mv ry rx; [ "$(cat rx)" = BBB ] && [ ! -e ry ] && P ir-rename-over || F ir-rename-over "$(cat rx)"
( exec 7>uf; echo LIVE>&7; rm uf; [ -e uf ]&&{ echo "FAIL ir-unlink-name::listed";exit;}; echo M>&7; exec 7>&-; echo "PASS ir-unlink-fd-write")
printf HELD>uf2; exec 8<uf2; rm uf2; r=$(cat<&8); exec 8<&-; [ "$r" = HELD ] && P ir-unlink-fd-read || F ir-unlink-fd-read "[$r]"
chmod 741 rx; [ "$(stat -c%a rx)" = 741 ] && P ir-chmod || F ir-chmod "$(stat -c%a rx)"
mkdir sub; echo z>sub/f; ls sub|grep -q '^f$' && P ir-readdir || F ir-readdir "[$(ls sub)]"
mkdir -p /root/w/bnd
echo FROMIR > $D/x1; mv $D/x1 /root/w/bnd/x1; [ "$(cat /root/w/bnd/x1)" = FROMIR ] && [ ! -e $D/x1 ] && P bnd-ir-to-overlay || F bnd-ir-to-overlay "$(cat /root/w/bnd/x1 2>&1)"
echo FROMOV > /root/w/bnd/x2; mv /root/w/bnd/x2 $D/x2; [ "$(cat $D/x2)" = FROMOV ] && [ ! -e /root/w/bnd/x2 ] && P bnd-overlay-to-ir || F bnd-overlay-to-ir "$(cat $D/x2 2>&1)"
cp /root/w/bnd/x1 $D/cp1 2>/dev/null && [ "$(cat $D/cp1)" = FROMIR ] && P bnd-cp-overlay-to-ir || F bnd-cp "$(cat $D/cp1 2>&1)"
cd $D; : > errors; mkdir -p cc ctmp
( for i in $(seq 1 300); do echo "v$i">ctmp/$i; mv ctmp/$i cc/f$i; done; : > cc/.done ) &
for r in 1 2 3 4 5 6; do
 ( while [ ! -e cc/.done ]; do for f in cc/f*; do [ "$f" = 'cc/f*' ]&&continue; if ! v=$(cat "$f" 2>/dev/null); then echo "READFAIL $f">>errors; elif [ -z "$v" ]; then echo "EMPTY $f">>errors; fi; done; done ) &
done
wait
for i in $(seq 1 300); do [ -r cc/f$i ] || echo "MISSING cc/f$i">>errors; done
c=$(ls cc|grep -c '^f'); [ "$c" = 300 ]||echo "COUNT $c">>errors
[ -s errors ] && { echo "IR-CONC-FAIL"; sort errors|uniq -c|head; } || echo IR-CONC-ALLOK
echo INRAMFS-DONE
'''

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
        def fresh():
            shutil.rmtree(work, ignore_errors=True)
            work.mkdir(parents=True, exist_ok=True)

        # Single-process POSIX semantics matrix (once — deterministic).
        fresh()
        r = box.run("SEM", SEMANTICS_WORKLOAD)
        for line in r.stdout.splitlines():
            if line.startswith("PASS "):
                check(True, "semantics: " + line[5:])
            elif line.startswith("FAIL "):
                check(False, "semantics: " + line[5:])
        check("SEMANTICS-DONE" in r.stdout,
              f"semantics workload ran to completion "
              f"(out={r.stdout.strip()[-200:]!r})")

        # inramfs (/tmp) semantics + overlay<->inramfs boundary + inramfs
        # concurrency — a distinct code path from the overlay above.
        fresh()
        r = box.run("IR", INRAMFS_WORKLOAD)
        for line in r.stdout.splitlines():
            if line.startswith("PASS "):
                check(True, "inramfs: " + line[5:])
            elif line.startswith("FAIL "):
                check(False, "inramfs: " + line[5:])
        check("IR-CONC-ALLOK" in r.stdout,
              f"inramfs concurrent visibility (6 readers, 300 files in /tmp) "
              f"(out={r.stdout.strip()[-160:]!r})")
        check("INRAMFS-DONE" in r.stdout,
              f"inramfs workload ran to completion "
              f"(out={r.stdout.strip()[-160:]!r})")

        # Concurrency — a race that fires 1-in-K only shows with repetition.
        for rnd in range(4):
            fresh()
            r = box.run(f"BG{rnd}", BG_WORKLOAD)
            check("BG-ALLOK" in r.stdout,
                  f"round {rnd}: background-job writes into a shared dir don't "
                  f"lose data (out={r.stdout.strip()[-160:]!r})")
            fresh()
            r = box.run(f"MK{rnd}", MAKE_WORKLOAD)
            check("MK-ALLOK" in r.stdout,
                  f"round {rnd}: make -j32 contention on a shared output dir "
                  f"(out={r.stdout.strip()[-160:]!r})")
            fresh()
            r = box.run(f"VIS{rnd}", VIS_WORKLOAD)
            check("VIS-ALLOK" in r.stdout,
                  f"round {rnd}: a listed stable file is always readable under "
                  f"6 concurrent readers (out={r.stdout.strip()[-160:]!r})")
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
