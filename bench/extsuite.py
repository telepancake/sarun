"""Shared plumbing for the external filesystem test batteries (pjdfstest, fsx).

These suites need a REAL kernel FUSE mount (they run external binaries that do
real syscalls). Like sarun itself — which declares its Python deps in a uv header
and installs them on run — the batteries PROVISION what they need and run, rather
than silently skipping: the suite source is fetched + built on demand (pinned to a
fixed revision; set SARUN_TEST_CACHE to relocate the cache, or PJDFSTEST_DIR /
FSX_BIN to point at a prebuilt copy), and the system tools they depend on
(fusermount3, prove, cc/make/git/autoconf) are apt-installed when missing
(`ensure_tools` / `require_*`, best-effort, needs root + apt).

A skip is therefore the EXCEPTION, reserved for what genuinely cannot be
provisioned: no root/apt to install with, or no /dev/fuse device (a kernel/
container capability, not a package). Those skips name the precise reason so a
"skipped" result is never mistaken for "verified".
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


# System tool -> the Debian package that provides it. Used to auto-provision the
# batteries' system prerequisites the way uv provisions sarun's Python deps.
_PKG_FOR = {
    "fusermount3": "fuse3",
    "bwrap":       "bubblewrap",
    "prove":       "perl",
    "cc":          "build-essential",
    "gcc":         "build-essential",
    "make":        "build-essential",
    "git":         "git",
    "autoreconf":  "autoconf automake",
    "automake":    "automake",
    "curl":        "curl",
    "pkg-config":  "pkg-config",
}
_apt_updated = False


def _can_apt():
    return os.geteuid() == 0 and shutil.which("apt-get") is not None


def _apt_install(pkgs):
    global _apt_updated
    if not _can_apt():
        return False
    env = dict(os.environ, DEBIAN_FRONTEND="noninteractive")
    if not _apt_updated:
        subprocess.run(["apt-get", "update"], env=env,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        _apt_updated = True
    print(f"  provisioning (apt-get install): {' '.join(pkgs)}")
    r = subprocess.run(["apt-get", "install", "-y", "--no-install-recommends", *pkgs],
                       env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    return r.returncode == 0


def ensure_tools(*tools):
    """Ensure each tool is on PATH, apt-installing its package if missing
    (best-effort). Returns the list still missing afterward (empty = all present)."""
    missing = [t for t in tools if not shutil.which(t)]
    if missing:
        pkgs = sorted({p for t in missing for p in _PKG_FOR.get(t, "").split()})
        if pkgs:
            _apt_install(pkgs)
        missing = [t for t in tools if not shutil.which(t)]
    return missing


def require_tools(*tools):
    """Provision the tools or skip() with a precise, honest reason."""
    missing = ensure_tools(*tools)
    if missing:
        why = "apt+root unavailable to install them" if not _can_apt() else "install failed"
        skip(f"missing {', '.join(missing)} ({why})")


def require_fuse():
    """A real FUSE mount needs fusermount3 (installable) and /dev/fuse (not — it's a
    kernel/container capability). Install the former; skip with a clear reason if the
    device is absent."""
    require_tools("fusermount3")
    if not os.path.exists("/dev/fuse"):
        skip("/dev/fuse missing — container has no FUSE device (cannot be apt-installed)")


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
        require_tools("git", "autoreconf", "make", "cc")
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
    require_tools("cc", "curl")
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
