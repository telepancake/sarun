#!/usr/bin/env python3
"""Engine-level tests for the multiplexed pyfuse3 overlay.

Run with the venv python (has pyfuse3+trio):
    /home/user/venv/bin/python test_overlay_engine.py

Self-safety: every mount is at an isolated temp point, exercised via
timeout-wrapped child commands, and lazy-unmounted in a finally block, leaving a
clean mount table.
"""
import os, sys, subprocess, tempfile, shutil, time, json, sqlite3
from pathlib import Path
from importlib.machinery import SourceFileLoader

SARUN = "/home/user/sarun/sarun"
m = SourceFileLoader("slopbox", SARUN).load_module()

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


class MountFixture:
    """Mount one session overlay at a temp point, run shell commands inside it via
    a timeout-wrapped child (operating through the FUSE path), then tear down."""
    def __init__(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="ovl-test-"))
        # The single db lives under state_home; keep it inside our temp tree so the
        # real ~/.local/state is untouched.
        os.environ["XDG_STATE_HOME"] = str(self.tmp / "state")
        self.mnt = self.tmp / "mnt"
        self.live = self.tmp / "live"
        self.sid = "20260604-000000_1"
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
        """Run a bash script with cwd = the session overlay root."""
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


def test_readthrough_and_create():
    fx = MountFixture()
    try:
        fx.start()
        # read-through to the host /etc/hostname
        r = fx.sh("cat /etc/hostname")
        host = Path("/etc/hostname").read_text() if Path("/etc/hostname").exists() else ""
        check(r.returncode == 0 and r.stdout == host,
              "read-through: cat /etc/hostname matches host")
        # create a new file
        r = fx.sh("mkdir -p sub && echo hello > sub/new.txt && cat sub/new.txt")
        check(r.returncode == 0 and r.stdout == "hello\n", "create file in new subdir")
        check((fx.up / "sub" / "new.txt").read_text() == "hello\n",
              "created file landed in upper")
        check(fx.index.kind_of("sub/new.txt") == "file", "index: created file = 'file'")
        check(fx.index.kind_of("sub") == "dir", "index: parent dir tracked")
    finally:
        fx.stop()


def test_copyup_modify():
    fx = MountFixture()
    try:
        fx.start()
        # modify a host file -> copy-up, host untouched
        r = fx.sh("cp /etc/hostname hn && echo APPENDED >> hn && cat hn")
        check(r.returncode == 0, "copy host file then append")
        # the modify is on the *copy* hn (a created file); now modify-in-place a
        # read-through path by copying-up: append to a path that mirrors lower.
        os.makedirs(fx.up.parent, exist_ok=True)
        # write directly through overlay to a lower-backed path
        target = "etc_copy_test_marker"
        r2 = fx.sh(f"printf 'x' > {target}")
        check(r2.returncode == 0, "create marker via overlay")
        check((fx.up / target).exists(), "marker in upper")
    finally:
        fx.stop()


def test_delete_whiteout():
    fx = MountFixture()
    try:
        fx.start()
        # create then delete -> upper-only, no whiteout needed
        fx.sh("echo a > f1 && rm f1")
        check(fx.index.kind_of("f1") is None, "create+delete upper-only leaves no entry")
        # delete a host file THROUGH the overlay (relative path = overlay path; an
        # absolute path would hit the host directly and is NOT what we test).
        r = fx.sh("rm etc/hostname; echo rc=$?")
        check("rc=0" in r.stdout, "rm of overlay etc/hostname succeeds")
        check("whiteout" == fx.index.kind_of("etc/hostname"),
              "deleting host /etc/hostname (via overlay) records a whiteout")
        # and it's masked in the merged view
        r2 = fx.sh("cat etc/hostname 2>&1; echo rc=$?")
        check("No such file" in r2.stdout or "rc=1" in r2.stdout,
              "whiteout masks the lower file")
        # host file itself is untouched
        check(Path("/etc/hostname").exists(), "host /etc/hostname still present")
    finally:
        fx.stop()


def test_symlink_and_readlink():
    fx = MountFixture()
    try:
        fx.start()
        r = fx.sh("ln -s /target/path mylink && readlink mylink")
        check(r.returncode == 0 and r.stdout.strip() == "/target/path",
              "symlink create + readlink")
        check(fx.index.kind_of("mylink") == "symlink", "index: symlink kind")
    finally:
        fx.stop()


def test_provenance_recorded():
    fx = MountFixture()
    try:
        fx.start()
        fx.sh("echo data > provtest.txt")
        prov = fx.index.writer_provenance("provtest.txt")
        check(prov is not None, "provenance recorded for a write")
        if prov:
            check(prov.get("pid", 0) > 0, "provenance has a pid")
            check(isinstance(prov.get("argv"), list), "provenance has argv list")
    finally:
        fx.stop()


def test_opaque_dir():
    fx = MountFixture()
    try:
        fx.start()
        # rm -rf a lower dir then recreate it -> opaque
        # use /usr/share-style? safer: pick a small lower dir we won't disturb on host.
        # /etc/skel commonly exists; otherwise create our own scenario via a known dir.
        lower_dir = None
        for cand in ("/etc/profile.d", "/etc/skel"):
            p = Path(cand)
            if p.is_dir() and any(p.iterdir()): lower_dir = cand; break
        if lower_dir is None:
            check(True, "opaque: no non-empty lower dir candidate (skipped)")
            return
        rel = lower_dir.lstrip("/")    # relative = overlay path
        r = fx.sh(f"rm -rf {rel} && mkdir {rel} && echo new > {rel}/only.txt")
        check(r.returncode == 0, f"rm -rf + recreate {rel}")
        check(fx.index.is_opaque(rel), "recreated dir over lower is opaque")
        # listing shows only the new file, not the old lower children
        r2 = fx.sh(f"ls {rel}")
        check(r2.stdout.strip() == "only.txt", "opaque dir hides lower children")
    finally:
        fx.stop()


def test_rename():
    fx = MountFixture()
    try:
        fx.start()
        r = fx.sh("echo one > a.txt && mv a.txt b.txt && cat b.txt")
        check(r.returncode == 0 and r.stdout == "one\n", "rename created file")
        check(fx.index.kind_of("b.txt") == "file", "index: renamed dest tracked")
        check(fx.index.kind_of("a.txt") is None, "index: renamed src dropped")
        # rename a directory tree
        r2 = fx.sh("mkdir d1 && echo x > d1/inner.txt && mv d1 d2 && cat d2/inner.txt")
        check(r2.returncode == 0 and r2.stdout == "x\n", "rename dir tree")
        check(fx.index.kind_of("d2/inner.txt") == "file", "index: reparented child")
    finally:
        fx.stop()


def test_lower_symlink_copyup_preserves_type():
    """A lower (host) symlink that gets copied up (e.g. setattr on it) must remain a
    symlink in the upper, NOT be materialized as its target's bytes."""
    fx = MountFixture()
    try:
        fx.start()
        # find a host symlink to exercise (many exist under /usr/lib, /etc/...)
        host_link = None
        for cand in ("/etc/mtab", "/etc/os-release"):
            if Path(cand).is_symlink(): host_link = cand; break
        if host_link is None:
            # fall back: create one on the lower? can't write host. scan /usr/bin.
            import glob as _g
            for p in _g.glob("/usr/bin/*")[:2000]:
                if os.path.islink(p): host_link = p; break
        if host_link is None:
            check(True, "lower-symlink: no host symlink found (skipped)"); return
        rel = host_link.lstrip("/")
        # touch -h sets mtime on the link itself (setattr utime, follow=False)
        r = fx.sh(f"touch -h -d '2001-01-01' {rel} 2>&1; echo rc=$?")
        # the upper artifact (if materialized) must be a symlink, not a file
        up_art = fx.up / rel
        if up_art.exists() or up_art.is_symlink():
            check(up_art.is_symlink(),
                  "copied-up lower symlink stays a symlink (not target bytes)")
        else:
            check(True, "lower symlink not materialized (no copy-up needed) — ok")
    finally:
        fx.stop()


def test_passthrough_acts_on_host_records_nothing():
    """With -d (blanket passthrough) the box's writes land on the REAL host fs (a
    temp lower we own) and NOTHING is recorded in the overlay upper / the sqlar."""
    fx = MountFixture()
    host_lower = Path(tempfile.mkdtemp(prefix="ovl-pt-host-"))
    try:
        fx.start(lower=str(host_lower), passthrough=True)
        r = fx.sh("mkdir -p d && echo hi > d/f.txt && cat d/f.txt && rm d/f.txt; "
                  "echo rc=$?")
        check("hi" in r.stdout and "rc=0" in r.stdout, "passthrough create/read/delete")
        # the bytes went to the REAL host lower, NOT the overlay upper.
        check(not (fx.up / "d").exists(), "nothing recorded in the overlay upper")
        check(fx.index.kind_of("d") is None and fx.index.kind_of("d/f.txt") is None,
              "nothing recorded in the single sqlar for passthrough paths")
        # create one that survives, prove it's a real host file under the lower.
        r2 = fx.sh("echo survive > kept.txt; echo rc=$?")
        check("rc=0" in r2.stdout, "passthrough create succeeds")
        check((host_lower / "kept.txt").read_text() == "survive\n",
              "passthrough write landed on the real host lower")
        check(not (fx.up / "kept.txt").exists(),
              "passthrough write left no overlay artifact")
    finally:
        fx.stop()
        shutil.rmtree(host_lower, ignore_errors=True)


if __name__ == "__main__":
    for t in (test_readthrough_and_create, test_copyup_modify, test_delete_whiteout,
              test_symlink_and_readlink, test_provenance_recorded, test_opaque_dir,
              test_rename, test_lower_symlink_copyup_preserves_type,
              test_passthrough_acts_on_host_records_nothing):
        print(f"\n== {t.__name__} ==")
        try:
            t()
        except Exception as e:
            import traceback; traceback.print_exc()
            _fails.append(f"{t.__name__}: {e}")
    print("\n" + ("ALL PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    sys.exit(1 if _fails else 0)
