#!/usr/bin/env python3
"""Coherence tests for the Tier-0 RAM write buffer under concurrent access.

The write buffer is keyed by (sid, rel) and holds the authoritative bytes of an
open-for-write file in RAM until release().  These tests check that *other*
accessors of the same path — a concurrent reader, an unlink, a rename — observe
the live buffered data and correct semantics, rather than EIO / stale bytes /
resurrected files.

Run with the venv python (has pyfuse3+trio):
    /home/user/venv/bin/python test_wbuf_coherence.py
"""
import os, sys, subprocess, tempfile, shutil
from pathlib import Path
from importlib.machinery import SourceFileLoader

SARUN = "/home/user/sarun/sarun"
m = SourceFileLoader("slopbox", SARUN).load_module()

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


class MountFixture:
    def __init__(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="ovl-wbuf-"))
        os.environ["XDG_STATE_HOME"] = str(self.tmp / "state")
        self.mnt = self.tmp / "mnt"
        self.live = self.tmp / "live"
        self.sid = "1"
        self.backing = self.live / self.sid
        self.up = self.backing / "up"
        self.up.mkdir(parents=True)
        self.mount = None
        self.index = None

    def start(self, lower=None, passthrough=False):
        self.index = m.Index(self.backing)
        self.mount = m.OverlayMount(self.mnt, lower=lower or "/")
        ok = self.mount.start()
        if not ok:
            raise RuntimeError(f"mount failed: {self.mount._start_error}")
        self.mount.add_session(self.sid, self.up, self.index,
                               passthrough=passthrough)
        self.root = self.mnt / self.sid

    def sh(self, script, timeout=15):
        return subprocess.run(["timeout", str(timeout), "bash", "-c", script],
                              cwd=str(self.root), capture_output=True, text=True)

    def stop(self):
        try:
            if self.mount: self.mount.stop()
        finally:
            try:
                if os.path.ismount(str(self.mnt)):
                    subprocess.run(["fusermount3", "-uz", str(self.mnt)],
                                   stdout=subprocess.DEVNULL,
                                   stderr=subprocess.DEVNULL, timeout=10)
            except Exception: pass
            try:
                if self.index: self.index.close()
            except Exception: pass
            shutil.rmtree(self.tmp, ignore_errors=True)


def test_concurrent_read_while_buffered():
    """A second, read-only open of a file still held open for write must see the
    bytes already written into the RAM buffer — not EIO and not stale content."""
    fx = MountFixture(); fx.start()
    try:
        # fd 3 stays open for write (buffer alive); read the path via a SECOND open
        # (cat) while fd 3 is still open. No sleep -> well under the 5s periodic flush,
        # so the row has not been snapshotted: the read must come from the buffer.
        r = fx.sh(
            "exec 3>newfile.txt; printf 'BUFFERED-PAYLOAD' >&3; "
            "out=$(cat newfile.txt); exec 3>&-; printf '%s' \"$out\"")
        check(r.returncode == 0,
              f"concurrent read: command succeeded (rc={r.returncode}, err={r.stderr!r})")
        check(r.stdout == "BUFFERED-PAYLOAD",
              f"concurrent read: reader sees buffered bytes (got {r.stdout!r})")
    finally:
        fx.stop()


def test_concurrent_read_existing_while_buffered():
    """Same, but over an existing host (lower) file rewritten via the buffer: the
    concurrent reader must see the NEW buffered content, not the old lower bytes."""
    fx = MountFixture(); fx.start()
    try:
        # seed an overlay file first (so it's captured, small, buffer-eligible)
        fx.sh("printf 'OLDOLDOLD' > existing.txt")
        r = fx.sh(
            "exec 3>existing.txt; printf 'NEWNEWNEWNEW' >&3; "
            "out=$(cat existing.txt); exec 3>&-; printf '%s' \"$out\"")
        check(r.returncode == 0,
              f"concurrent read existing: succeeded (rc={r.returncode}, err={r.stderr!r})")
        check(r.stdout == "NEWNEWNEWNEW",
              f"concurrent read existing: reader sees new buffered bytes (got {r.stdout!r})")
    finally:
        fx.stop()


def test_unlink_open_buffer_no_resurrect():
    """Unlinking a file that is still open in a write buffer must NOT resurrect it
    when the writer later closes (release() must not re-create the row)."""
    fx = MountFixture(); fx.start()
    try:
        r = fx.sh(
            "exec 3>victim.txt; printf 'DATA' >&3; rm victim.txt; exec 3>&-; "
            "test -e victim.txt && echo PRESENT || echo GONE")
        check(r.returncode == 0,
              f"unlink-open: command ran (rc={r.returncode}, err={r.stderr!r})")
        check(r.stdout.strip() == "GONE",
              f"unlink-open: file stays deleted after close (got {r.stdout.strip()!r})")
        check(fx.index.kind_of("victim.txt") in (None, "whiteout"),
              "unlink-open: index has no resurrected file row")
    finally:
        fx.stop()


def test_rename_open_buffer_follows():
    """Renaming a file that is still open in a write buffer must carry the data to
    the new name; the old name must not reappear after the writer closes."""
    fx = MountFixture(); fx.start()
    try:
        r = fx.sh(
            "exec 3>src.txt; printf 'MOVEME' >&3; mv src.txt dst.txt; exec 3>&-; "
            "printf 'src=%s dst=%s ' "
            "\"$(test -e src.txt && echo Y || echo N)\" "
            "\"$(cat dst.txt 2>/dev/null)\"")
        check(r.returncode == 0,
              f"rename-open: command ran (rc={r.returncode}, err={r.stderr!r})")
        check("src=N" in r.stdout,
              f"rename-open: old name gone after close (got {r.stdout!r})")
        check("dst=MOVEME" in r.stdout,
              f"rename-open: data landed at new name (got {r.stdout!r})")
    finally:
        fx.stop()


def test_parallel_write_read_rename_unlink():
    """Build-like stress: many concurrent workers each write a temp file, read it
    back through a second fd while still open, rename it into place, then a reader
    verifies final content. Exercises the buffer coherence seams under real
    parallelism. Every worker must end with the exact bytes it wrote."""
    fx = MountFixture(); fx.start()
    try:
        n = 24
        script = r'''
work() {
  i="$1"
  payload="payload-$i-$(printf '%0.sX' $(seq 1 $i))"
  # write via a held-open fd, read back through a SECOND open while still open
  exec 9>"build/obj_$i.tmp"
  printf '%s' "$payload" >&9
  got_inflight="$(cat "build/obj_$i.tmp")"
  exec 9>&-
  mv "build/obj_$i.tmp" "build/obj_$i.o"
  got_final="$(cat "build/obj_$i.o")"
  # a throwaway temp that gets unlinked while open must not linger
  exec 8>"build/scratch_$i"; printf 'scratch' >&8; rm "build/scratch_$i"; exec 8>&-
  if [ "$got_inflight" = "$payload" ] && [ "$got_final" = "$payload" ] \
       && [ ! -e "build/scratch_$i" ]; then
    echo "ok $i"
  else
    echo "BAD $i inflight=[$got_inflight] final=[$got_final] scratch=$([ -e build/scratch_$i ] && echo present || echo gone)"
  fi
}
mkdir -p build
for i in $(seq 1 __N__); do work "$i" & done
wait
'''.replace("__N__", str(n))
        r = fx.sh(script, timeout=60)
        oks = sum(1 for ln in r.stdout.splitlines() if ln.startswith("ok "))
        bads = [ln for ln in r.stdout.splitlines() if ln.startswith("BAD")]
        check(r.returncode == 0, f"parallel: workers completed (rc={r.returncode})")
        check(oks == n and not bads,
              f"parallel: all {n} workers coherent (ok={oks}, bad={bads[:3]})")
    finally:
        fx.stop()


if __name__ == "__main__":
    for fn in (test_concurrent_read_while_buffered,
               test_concurrent_read_existing_while_buffered,
               test_unlink_open_buffer_no_resurrect,
               test_rename_open_buffer_follows,
               test_parallel_write_read_rename_unlink):
        print(f"\n== {fn.__name__} ==")
        try:
            fn()
        except Exception as e:
            check(False, f"{fn.__name__} raised {type(e).__name__}: {e}")
    print()
    if _fails:
        print(f"{len(_fails)} FAILURE(S)")
        sys.exit(1)
    print("ALL OK")
