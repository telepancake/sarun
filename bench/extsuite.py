"""Shared plumbing for the external filesystem test batteries (pjdfstest, fsx).

These suites need a REAL kernel FUSE mount (they run external binaries that do
real syscalls), so unlike the in-process overlay tests they require fusermount3 +
/dev/fuse and skip cleanly when those (or the build toolchain / network) are
missing. The suites are fetched + built into a cache on first use, pinned to a
fixed revision for determinism; set SARUN_TEST_CACHE to relocate it, or point
PJDFSTEST_DIR / FSX_BIN at a prebuilt copy to skip the build.
"""
import os
import shutil
import subprocess
import sys
import tempfile
import contextlib
from importlib.machinery import SourceFileLoader
from pathlib import Path

SARUN = os.environ.get("SARUN_PATH",
                       str(Path(__file__).resolve().parent.parent / "sarun"))
CACHE = Path(os.environ.get("SARUN_TEST_CACHE",
                            os.path.join(tempfile.gettempdir(), "sarun-test-suites")))

# Pinned revisions — keep the test set stable across runs/machines.
PJDFSTEST_URL = "https://github.com/pjd/pjdfstest"
PJDFSTEST_COMMIT = "ededbeb2b44929972898afb87474b0937f78a877"
# Old LTP fsx-linux predates the tst_test harness, so it builds standalone.
FSX_URL = ("https://raw.githubusercontent.com/linux-test-project/ltp/"
           "20120401/testcases/kernel/fs/fsx-linux/fsx-linux.c")


class _Skip(Exception):
    pass


def skip(msg):
    """Soft skip: print a SKIP line, and raise pytest.Skipped only when actually
    running under pytest (so standalone `python test_x.py` just returns/passes)."""
    print(f"SKIP: {msg}")
    if "pytest" in sys.modules:
        import pytest
        pytest.skip(msg)
    raise _Skip(msg)


def have(*tools):
    return all(shutil.which(t) for t in tools)


def fuse_available():
    return os.path.exists("/dev/fuse") and bool(shutil.which("fusermount3"))


def load_sarun():
    # load_module() registers in sys.modules (needed for @dataclass introspection).
    return SourceFileLoader("sarunmod", SARUN).load_module()


@contextlib.contextmanager
def overlay_session():
    """Mount the real multiplexed overlay under a throwaway XDG root, register one
    session, and yield its box root (the merged view of /). Tears the mount down on
    exit. Raises if the mount can't come up."""
    sarun = load_sarun()
    tmproot = tempfile.mkdtemp(prefix="extsuite-")
    os.environ["XDG_STATE_HOME"] = os.path.join(tmproot, "state")
    os.environ["XDG_RUNTIME_DIR"] = os.path.join(tmproot, "run")
    os.makedirs(os.environ["XDG_STATE_HOME"], exist_ok=True)
    os.makedirs(os.environ["XDG_RUNTIME_DIR"], exist_ok=True)
    mount = sarun.OverlayMount(sarun.mnt_point(), lower="/")
    if not mount.start():
        shutil.rmtree(tmproot, ignore_errors=True)
        raise RuntimeError(f"overlay mount failed: {mount._start_error}")
    try:
        sid = str(sarun.mint_box_id())
        backing = sarun.live_dir(sid)
        (backing / "up").mkdir(parents=True, exist_ok=True)
        mount.add_session(sid, backing / "up", sarun.Index(backing))
        yield str(sarun.mnt_point() / sid)
    finally:
        try:
            mount.stop()
        except Exception:
            pass
        shutil.rmtree(tmproot, ignore_errors=True)


def _git_checkout(url, commit, dest):
    if not (dest / ".git").exists():
        CACHE.mkdir(parents=True, exist_ok=True)
        subprocess.run(["git", "clone", url, str(dest)], check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    subprocess.run(["git", "-C", str(dest), "checkout", "-q", commit], check=True)


def ensure_pjdfstest():
    """Return (tests_dir, binary) for a built pjdfstest, or skip()."""
    pre = os.environ.get("PJDFSTEST_DIR")
    if pre:
        root = Path(pre)
    else:
        if not have("git", "autoreconf", "make", "cc"):
            skip("pjdfstest build needs git/autoreconf/make/cc")
        root = CACHE / "pjdfstest"
        try:
            _git_checkout(PJDFSTEST_URL, PJDFSTEST_COMMIT, root)
        except subprocess.CalledProcessError as e:
            skip(f"could not fetch pjdfstest ({e})")
    binary = root / "pjdfstest"
    if not (binary.exists() and os.access(binary, os.X_OK)):
        log = root / "build.log"
        try:
            with open(log, "w") as f:
                subprocess.run(["autoreconf", "-ifs"], cwd=root, check=True,
                               stdout=f, stderr=f)
                subprocess.run(["./configure"], cwd=root, check=True,
                               stdout=f, stderr=f)
                subprocess.run(["make", "pjdfstest"], cwd=root, check=True,
                               stdout=f, stderr=f)
        except subprocess.CalledProcessError:
            skip(f"pjdfstest build failed (see {log})")
    return root / "tests", binary


def ensure_fsx():
    """Return a built fsx binary path, or skip()."""
    pre = os.environ.get("FSX_BIN")
    if pre and os.access(pre, os.X_OK):
        return Path(pre)
    if not have("cc", "curl"):
        skip("fsx build needs cc + curl")
    CACHE.mkdir(parents=True, exist_ok=True)
    src = CACHE / "fsx-linux.c"
    binary = CACHE / "fsx"
    if not binary.exists():
        if not src.exists():
            r = subprocess.run(["curl", "-fsSL", FSX_URL, "-o", str(src)])
            if r.returncode != 0:
                skip("could not fetch fsx source")
        r = subprocess.run(["cc", "-O2", "-o", str(binary), str(src)])
        if r.returncode != 0:
            skip("fsx build failed")
    return binary
