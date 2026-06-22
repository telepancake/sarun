import re
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
from pathlib import Path

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




# ── pjdfstest result parsing (moved here from the deleted test_pjdfstest.py) ──
GROUPS = ["open", "truncate", "ftruncate", "unlink", "mkdir", "rmdir",
          "rename", "symlink", "link", "utimensat", "chmod", "mkfifo"]


def _expand(spec):
    """'18-23, 25-27, 33' -> [18,19,20,21,22,23,25,26,27,33]"""
    out = []
    for part in spec.replace(",", " ").split():
        if "-" in part:
            a, b = part.split("-", 1)
            if a.isdigit() and b.isdigit():
                out.extend(range(int(a), int(b) + 1))
        elif part.isdigit():
            out.append(int(part))
    return out


def parse_failures(output):
    """Per-assertion signatures: {'open/07.t#3', ...}. A file that ends dubious
    (non-zero wait status — a crash/hang) yields 'group/NN.t#WSTAT'."""
    sigs = set()
    cur = None
    collecting = False
    file_re = re.compile(r"/tests/(\S+\.t)\s+\(Wstat:\s*(\d+).*?Failed:\s*(\d+)?")
    failed_re = re.compile(r"^\s*Failed tests?:\s*(.*)$")
    cont_re = re.compile(r"^\s+[\d,\-\s]+$")
    for line in output.splitlines():
        m = file_re.search(line)
        if m:
            cur = m.group(1)
            collecting = False
            if m.group(2) != "0":
                sigs.add(f"{cur}#WSTAT")
            continue
        fm = failed_re.match(line)
        if fm and cur:
            collecting = True
            for n in _expand(fm.group(1)):
                sigs.add(f"{cur}#{n}")
            continue
        if collecting and cur and line.strip() and cont_re.match(line):
            for n in _expand(line):
                sigs.add(f"{cur}#{n}")
            continue
        collecting = False
    return sigs
