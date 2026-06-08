#!/usr/bin/env python3
"""Contract spec for the open-for-write path — behaviour must NOT depend on how
the file happens to be stored (RAM write-buffer vs on-disk pool blob).

Every scenario runs at TWO sizes: one comfortably under the Tier-0 spill
threshold (lives in the RAM buffer) and one comfortably over it (takes the
on-disk copy-up path).  The observable result — whether a change is recorded,
and the resulting mtime — MUST be identical.  Divergence here is the size-
dependent heisenbug class: "works at 900 KB, misbehaves at 1.1 MB".

Status: the SMALL cases pass today; the LARGE "clean open records nothing" case
FAILS, because the disk path copies up eagerly at open() and never reverts.
This file is the spec the unified write path must satisfy.

Run with the venv python (has pyfuse3+trio):
    /home/user/venv/bin/python test_write_path_contract.py
"""
import os, sys, subprocess, tempfile, shutil, time
from pathlib import Path
from importlib.machinery import SourceFileLoader

SARUN = "/home/user/sarun/sarun"
m = SourceFileLoader("slopbox", SARUN).load_module()

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)

DAY_NS = 24 * 3600 * 1_000_000_000
OLD_NS = time.time_ns() - 5 * DAY_NS
SLACK_NS = 2 * 1_000_000_000

# Straddle the Tier-0 spill threshold (WBUF_SPILL = 1 MiB) so the two sizes take
# the two storage paths the contract is meant to unify.
SMALL = 4 << 10          # 4 KiB  -> RAM write buffer
LARGE = 2 << 20          # 2 MiB  -> on-disk copy-up / spill
SIZES = (("small", SMALL), ("large", LARGE))


def _content(size):
    """Deterministic, exact-length bytes for a seeded file."""
    return (b"".join(bytes([i & 0xFF]) for i in range(256)) * (size // 256)
            + b"\0" * (size % 256))


def _is_old(mtime_ns):
    return abs(mtime_ns - OLD_NS) <= SLACK_NS


def settle(pred, timeout=5.0):
    end = time.time() + timeout
    while time.time() < end:
        try:
            if pred(): return True
        except Exception: pass
        time.sleep(0.01)
    try: return bool(pred())
    except Exception: return False


class MountFixture:
    def __init__(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="ovl-contract-"))
        os.environ["XDG_STATE_HOME"] = str(self.tmp / "state")
        self.mnt = self.tmp / "mnt"
        self.live = self.tmp / "live"
        self.lower = self.tmp / "lower"
        self.lower.mkdir(parents=True)
        self.sid = "1"
        self.backing = self.live / self.sid
        self.up = self.backing / "up"
        self.up.mkdir(parents=True)
        self.mount = None
        self.index = None

    def seed_lower(self, rel, size, mtime_ns=OLD_NS, mode=0o644):
        p = self.lower / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(_content(size))
        os.chmod(p, mode)
        os.utime(p, ns=(mtime_ns, mtime_ns))
        return p

    def start(self):
        self.index = m.Index(self.backing)
        self.mount = m.OverlayMount(self.mnt, lower=str(self.lower))
        if not self.mount.start():
            raise RuntimeError(f"mount failed: {self.mount._start_error}")
        self.mount.add_session(self.sid, self.up, self.index)
        self.root = self.mnt / self.sid

    def stat(self, rel):
        return os.stat(self.root / rel)

    def captured(self, rel):
        return self.index.kind_of(rel) == "file"

    def sh(self, script, timeout=30):
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


def contract_clean_open_records_nothing(label, size):
    """Open O_RDWR, read the whole file, close — never write.  No change recorded,
    mtime untouched.  Identical for RAM and blob storage."""
    fx = MountFixture(); fx.seed_lower("f.dat", size); fx.start()
    try:
        r = fx.sh("exec 3<>f.dat; cat <&3 >/dev/null; exec 3>&-")
        check(r.returncode == 0, f"[{label}] clean-open ran (err={r.stderr!r})")
        ok = settle(lambda: not fx.captured("f.dat"))
        check(ok, f"[{label}] clean open records NO change "
                  f"(captured={fx.captured('f.dat')})")
        check(_is_old(fx.stat("f.dat").st_mtime_ns),
              f"[{label}] clean open preserves mtime "
              f"(got {fx.stat('f.dat').st_mtime_ns})")
    finally:
        fx.stop()


def contract_write_records_change(label, size):
    """A real write is always recorded and advances mtime, at any size."""
    fx = MountFixture(); fx.seed_lower("f.dat", size); fx.start()
    try:
        t0 = time.time_ns()
        r = fx.sh("printf 'APPENDED' >> f.dat")
        check(r.returncode == 0, f"[{label}] write ran (err={r.stderr!r})")
        check(settle(lambda: fx.captured("f.dat")),
              f"[{label}] write records a change")
        check(fx.stat("f.dat").st_mtime_ns >= t0 - SLACK_NS,
              f"[{label}] write advances mtime to now")
        check((fx.root / "f.dat").read_bytes().endswith(b"APPENDED"),
              f"[{label}] appended bytes visible")
    finally:
        fx.stop()


def contract_truncate_records_change(label, size):
    """O_TRUNC is a modification (truncate to empty) — recorded, mtime advanced."""
    fx = MountFixture(); fx.seed_lower("f.dat", size); fx.start()
    try:
        t0 = time.time_ns()
        r = fx.sh(": > f.dat")
        check(r.returncode == 0, f"[{label}] truncate ran (err={r.stderr!r})")
        check(settle(lambda: fx.captured("f.dat")),
              f"[{label}] truncate records a change")
        check(fx.stat("f.dat").st_mtime_ns >= t0 - SLACK_NS,
              f"[{label}] truncate advances mtime")
        check((fx.root / "f.dat").read_bytes() == b"",
              f"[{label}] file is empty after truncate")
    finally:
        fx.stop()


def contract_read_modify_write(label, size):
    """open O_RDWR, read part, then overwrite a middle slice in place — the rest of
    the file (seeded from the source) is preserved, size is unchanged, the change is
    recorded and mtime advances. For the large size this exercises blob-backed
    materialization on first write; for the small size, the RAM path."""
    fx = MountFixture(); fx.seed_lower("f.dat", size); fx.start()
    try:
        t0 = time.time_ns()
        # read the head through an O_RDWR fd, then patch 4 bytes at offset 100.
        r = fx.sh("exec 3<>f.dat; head -c 16 <&3 >/dev/null; "
                  "printf '\\xff\\xff\\xff\\xff' | "
                  "dd of=f.dat bs=1 seek=100 conv=notrunc status=none; exec 3>&-")
        check(r.returncode == 0, f"[{label}] rmw ran (err={r.stderr!r})")
        check(settle(lambda: fx.captured("f.dat")), f"[{label}] rmw records a change")
        got = (fx.root / "f.dat").read_bytes()
        want = bytearray(_content(size)); want[100:104] = b"\xff\xff\xff\xff"
        check(len(got) == size, f"[{label}] rmw preserves size ({len(got)} vs {size})")
        check(got == bytes(want),
              f"[{label}] rmw patched the slice and preserved the rest")
        check(fx.stat("f.dat").st_mtime_ns >= t0 - SLACK_NS,
              f"[{label}] rmw advances mtime")
    finally:
        fx.stop()


if __name__ == "__main__":
    for fn in (contract_clean_open_records_nothing,
               contract_write_records_change,
               contract_truncate_records_change,
               contract_read_modify_write):
        for label, size in SIZES:
            print(f"\n== {fn.__name__} [{label}] ==")
            try:
                fn(label, size)
            except Exception as e:
                check(False, f"{fn.__name__}[{label}] raised {type(e).__name__}: {e}")
    print()
    if _fails:
        print(f"{len(_fails)} FAILURE(S)")
        sys.exit(1)
    print("ALL OK")
