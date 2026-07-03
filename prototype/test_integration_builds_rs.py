#!/usr/bin/env python3
"""Real-project integration builds through -b boxes.

Two end-to-end builds, each in its own real box (FUSE overlay + bwrap, the
engine shadowing make/sh/bash), asserting the built program runs and prints
its expected output INSIDE the box:

  hello:  GNU hello 2.12.1 — `./configure && make && ./hello`
          (autoconf configure is the harshest realistic shell workload:
          traps, LINENO, sed pipelines, heredocs, `make -f -` depfiles)
  cmake:  a generated CMakeLists project — `cmake -G "Unix Makefiles" .
          && make && ./demo`

The hello tarball is cached under ~/.cache/sarun-integ/ and fetched from
ftp.gnu.org on first run; the case skips (vacuously passes) if the tarball
is absent and unfetchable. The cmake case skips if cmake is not installed.

Run:
    uv run --with "pyfuse3>=3.2" --with "trio>=0.22" --with "wcmatch>=8.4" \
      --with "python-magic>=0.4" python test_integration_builds_rs.py
Skips (passes vacuously) if the engine binary is unavailable.
"""
import os, shutil, socket, subprocess, sys, tarfile, tempfile, time, urllib.request
from pathlib import Path
from importlib.machinery import SourceFileLoader

_HERE = Path(__file__).resolve().parent
SARUN = str(_HERE / "libtestsarun.py")
BIN = _HERE.parent / "engine/target/x86_64-unknown-linux-musl/release/sarun"

HELLO_URL = "https://ftp.gnu.org/gnu/hello/hello-2.12.1.tar.gz"
CACHE = Path.home() / ".cache" / "sarun-integ"

try:  # real builds: give this file more than the suite's 180s default
    import pytest
    pytestmark = pytest.mark.timeout(900)
except ImportError:
    pass

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)

def wait_socket_path(p, timeout=15.0):
    end = time.time() + timeout
    while time.time() < end:
        if os.path.exists(p):
            s = socket.socket(socket.AF_UNIX)
            try:
                s.connect(str(p)); return True
            except OSError:
                pass
            finally:
                s.close()
        time.sleep(0.1)
    return False

def hello_tarball():
    CACHE.mkdir(parents=True, exist_ok=True)
    tb = CACHE / "hello-2.12.1.tar.gz"
    if tb.exists():
        return tb
    try:
        with urllib.request.urlopen(HELLO_URL, timeout=60) as r, \
             open(tb, "wb") as f:
            shutil.copyfileobj(r, f)
        return tb
    except OSError as e:
        tb.unlink(missing_ok=True)
        print(f"  skip hello: tarball unfetchable ({e})")
        return None

CMAKELISTS = """cmake_minimum_required(VERSION 3.10)
project(demo C)
add_executable(demo main.c)
"""
MAIN_C = """#include <stdio.h>
int main(void) { puts("demo: Hello, world!"); return 0; }
"""

def run_box(name, cwd, script, timeout):
    """Run `sh -c script` in a fresh -b box rooted at cwd; return the
    CompletedProcess (stdout is the box's logical stdout)."""
    return subprocess.run(
        [str(BIN), "run", "-b", name, "-C", str(cwd), "--", "sh", "-c", script],
        capture_output=True, text=True, timeout=timeout)

def main():
    if not BIN.exists():
        print("test_integration_builds_rs: no engine binary (skip)")
        return 0
    tmp = Path(tempfile.mkdtemp(prefix="integ-xdg-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
        (tmp / sub).mkdir(parents=True, exist_ok=True)
    os.environ["SLOPBOX_NS"] = "IB"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()

    # box-visible work root (host /tmp is tmpfs and hidden box-side)
    work = Path("/root/integ_builds_work")
    shutil.rmtree(work, ignore_errors=True)
    work.mkdir(parents=True)

    eng = subprocess.Popen([str(BIN), "serve"],
                           stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)
    try:
        if not wait_socket_path(m.sock_path()):
            check(False, "engine socket appeared")
            return 1

        # ── GNU hello: configure && make && ./hello ────────────────────────
        tb = hello_tarball()
        if tb:
            with tarfile.open(tb) as t:
                t.extractall(work)
            src = work / "hello-2.12.1"
            r = run_box("HELLO", src,
                        "./configure >conf.log 2>&1 && make >make.log 2>&1 "
                        "&& ./hello", timeout=900)
            check(r.returncode == 0,
                  f"hello box exits 0 (got {r.returncode}: {r.stderr[-400:]})")
            check("Hello, world!" in r.stdout,
                  f"./hello prints greeting (got {r.stdout[-200:]!r})")

        # ── cmake: generate && make && ./demo ──────────────────────────────
        if shutil.which("cmake"):
            src = work / "cmakeproj"
            src.mkdir()
            (src / "CMakeLists.txt").write_text(CMAKELISTS)
            (src / "main.c").write_text(MAIN_C)
            r = run_box("CMAKE", src,
                        'cmake -G "Unix Makefiles" . >cm.log 2>&1 '
                        "&& make >make.log 2>&1 && ./demo", timeout=600)
            check(r.returncode == 0,
                  f"cmake box exits 0 (got {r.returncode}: {r.stderr[-400:]})")
            check("demo: Hello, world!" in r.stdout,
                  f"./demo prints greeting (got {r.stdout[-200:]!r})")
        else:
            print("  skip cmake: not installed")
    finally:
        eng.terminate()
        eng.wait(timeout=10)
        shutil.rmtree(work, ignore_errors=True)

    if _fails:
        print(f"\n{len(_fails)} FAILURES"); return 1
    print("\nall integration builds ok"); return 0

def test_integration_builds_rs():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
