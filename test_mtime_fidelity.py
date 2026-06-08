#!/usr/bin/env python3
"""Regression tests: the overlay must not fudge a file's mtime.

A file that the box only READS, merely OPENS for write, or CHMODs must keep its
ORIGINAL mtime.  Only a real content modification — a write or a truncate — may
advance mtime to "now".  chmod must touch ctime only (POSIX), never mtime.

Why this matters (the bug these guard against): copy-up is supposed to be a
transparent implementation detail, but it used to stamp the freshly-created pool
blob with its own creation time, and the Tier-0 write buffer seeded its mtime
from the wall clock at open() rather than from the source.  Either path made a
shipped, unmodified file look freshly modified inside the box.  In an autotools
tree that inverts the mtime ordering the maintainer-mode rebuild rules rely on,
so `make` tries to re-run aclocal/automake/autoconf (often absent in the box)
and the build fails — while a direct build, which never copies up, succeeds.

Run with the venv python (has pyfuse3+trio):
    /home/user/venv/bin/python test_mtime_fidelity.py
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


# A timestamp safely in the past, so a "bumped to now" regression shows up as a
# multi-day jump that no filesystem-precision slack can mask.
DAY_NS = 24 * 3600 * 1_000_000_000
OLD_NS = time.time_ns() - 5 * DAY_NS
# Generous slack absorbing any coarse-mtime filesystem under the pool/temp dir
# while still being a tiny fraction of the 5-day gap to "now".
SLACK_NS = 2 * 1_000_000_000


class MountFixture:
    """One session overlay over a CONTROLLED lower dir (so we own the seed mtimes),
    mounted at a temp point and driven through the FUSE path by child commands."""
    def __init__(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="ovl-mtime-"))
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

    def seed_lower(self, rel, content=b"data\n", mtime_ns=OLD_NS, mode=0o644):
        """Create a lower-layer file with an exact, old mtime."""
        p = self.lower / rel
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(content)
        os.chmod(p, mode)
        os.utime(p, ns=(mtime_ns, mtime_ns))
        return p

    def start(self):
        self.index = m.Index(self.backing)
        self.mount = m.OverlayMount(self.mnt, lower=str(self.lower))
        ok = self.mount.start()
        if not ok:
            raise RuntimeError(f"mount failed: {self.mount._start_error}")
        self.mount.add_session(self.sid, self.up, self.index)
        self.root = self.mnt / self.sid

    def stat(self, rel):
        return os.stat(self.root / rel)

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


def _is_old(mtime_ns):
    return abs(mtime_ns - OLD_NS) <= SLACK_NS


def test_readonly_preserves_mtime():
    """Baseline: a file the box only reads reports its real lower mtime."""
    fx = MountFixture()
    fx.seed_lower("README", b"hello\n")
    fx.start()
    try:
        r = fx.sh("cat README >/dev/null")
        check(r.returncode == 0, f"readonly: cat ran (err={r.stderr!r})")
        check(_is_old(fx.stat("README").st_mtime_ns),
              f"readonly: mtime unchanged (got {fx.stat('README').st_mtime_ns}, want ~{OLD_NS})")
    finally:
        fx.stop()


def test_chmod_preserves_mtime_bumps_ctime():
    """chmod must change mode + ctime only — never mtime.  This is the headline
    regression: a `chmod +x` (config.status / libtool / automake do this on
    shipped files) used to copy the file up with mtime = now."""
    fx = MountFixture()
    fx.seed_lower("configure", b"#!/bin/sh\n", mode=0o644)
    fx.start()
    try:
        t0 = time.time_ns()
        r = fx.sh("chmod +x configure")
        check(r.returncode == 0, f"chmod: ran (err={r.stderr!r})")
        st = fx.stat("configure")
        check(st.st_mode & 0o111, "chmod: exec bit applied")
        check(_is_old(st.st_mtime_ns),
              f"chmod: mtime PRESERVED (got {st.st_mtime_ns}, want ~{OLD_NS})")
        check(st.st_ctime_ns >= t0 - SLACK_NS,
              f"chmod: ctime advanced to now (got {st.st_ctime_ns}, t0={t0})")
        check(st.st_ctime_ns > st.st_mtime_ns,
              "chmod: ctime is newer than the preserved mtime")
    finally:
        fx.stop()


def test_open_for_write_no_write_preserves_mtime():
    """Opening a small file O_RDWR and closing it WITHOUT writing must not advance
    mtime.  The Tier-0 write buffer used to seed its mtime from the wall clock and
    persist that into the sqlar row on release, so a clean open-for-write of a
    shipped file recorded a bogus "now".  We assert on the PERSISTED row mtime
    directly — the kernel attr cache (ATTR_TIMEOUT) masks this through stat(), but
    the row is what consolidation and a post-cache-expiry getattr report."""
    fx = MountFixture()
    fx.seed_lower("aclocal.m4", b"dnl generated\n")
    fx.start()
    try:
        # bash `<>` opens O_RDWR (no truncate); we write nothing before closing.
        r = fx.sh("exec 3<>aclocal.m4; exec 3>&-")
        check(r.returncode == 0, f"open-no-write: ran (err={r.stderr!r})")
        row = fx.index.file_row("aclocal.m4")    # (rowid, mode, sz, mtime, data)
        check(row is not None, "open-no-write: file captured in the overlay")
        if row is not None:
            check(_is_old(row[3]),
                  f"open-no-write: persisted mtime is the source time, not now "
                  f"(got {row[3]}, want ~{OLD_NS})")
    finally:
        fx.stop()


def test_real_write_advances_mtime():
    """Sanity: a genuine content write DOES advance mtime to now — the fix must
    not freeze mtimes, only stop spurious bumps."""
    fx = MountFixture()
    fx.seed_lower("config.h", b"old\n")
    fx.start()
    try:
        t0 = time.time_ns()
        r = fx.sh("printf 'new\\n' >> config.h")
        check(r.returncode == 0, f"write: ran (err={r.stderr!r})")
        mt = fx.stat("config.h").st_mtime_ns
        check(mt >= t0 - SLACK_NS,
              f"write: mtime advanced to now (got {mt}, t0={t0})")
        check(not _is_old(mt), "write: mtime is no longer the old seed value")
    finally:
        fx.stop()


def test_truncate_open_advances_mtime():
    """Opening with O_TRUNC IS a modification, so mtime must advance even before
    any bytes are written."""
    fx = MountFixture()
    fx.seed_lower("stamp", b"xxxxxxxx\n")
    fx.start()
    try:
        t0 = time.time_ns()
        r = fx.sh(": > stamp")          # `: >` truncates, writes nothing
        check(r.returncode == 0, f"trunc: ran (err={r.stderr!r})")
        mt = fx.stat("stamp").st_mtime_ns
        check(mt >= t0 - SLACK_NS,
              f"trunc: mtime advanced to now (got {mt}, t0={t0})")
    finally:
        fx.stop()


def test_dependency_ordering_survives_chmod():
    """The actual autotools failure mode, distilled: a generated TARGET (aclocal.m4)
    ships strictly newer than its PREREQUISITE (configure.ac).  chmod-ing the
    target inside the box must NOT make the prerequisite look newer — otherwise
    `make` decides the target is stale and re-runs the (absent) aclocal."""
    fx = MountFixture()
    fx.seed_lower("configure.ac", mtime_ns=OLD_NS)              # prerequisite (older)
    fx.seed_lower("aclocal.m4",  mtime_ns=OLD_NS + 10 * 1_000_000_000)  # target (newer)
    fx.start()
    try:
        # Something touches the target's mode (e.g. a read-modify chmod during build).
        r = fx.sh("chmod 0644 aclocal.m4")
        check(r.returncode == 0, f"ordering: chmod ran (err={r.stderr!r})")
        prereq = fx.stat("configure.ac").st_mtime_ns
        target = fx.stat("aclocal.m4").st_mtime_ns
        check(target > prereq,
              f"ordering: target stays newer than prerequisite "
              f"(target={target}, prereq={prereq}) — aclocal stays dormant")
    finally:
        fx.stop()


if __name__ == "__main__":
    for fn in (test_readonly_preserves_mtime,
               test_chmod_preserves_mtime_bumps_ctime,
               test_open_for_write_no_write_preserves_mtime,
               test_real_write_advances_mtime,
               test_truncate_open_advances_mtime,
               test_dependency_ordering_survives_chmod):
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
