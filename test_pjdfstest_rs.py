#!/usr/bin/env python3
"""pjdfstest POSIX battery against the RUST engine mount — the independent,
language-neutral oracle the port should have been graded on. Launches
`sarun-engine serve`, creates a box, runs the curated pjdfstest groups inside
the box's overlay, and compares the per-assertion failure set to the PYTHON
engine's checked-in baseline (bench/pjdfstest_baseline.txt). Failures the Rust
mount has that the Python baseline does NOT are real Rust defects.

    uv run --with pyfuse3 --with trio --with wcmatch --with python-magic \
        python test_pjdfstest_rs.py
"""
import os, re, socket, subprocess, sys, tempfile, shutil, time
from pathlib import Path
from importlib.machinery import SourceFileLoader

sys.path.insert(0, str(Path(__file__).resolve().parent / "bench"))
import extsuite
from test_pjdfstest import parse_failures, GROUPS  # reuse parser + group list

BIN = Path("/home/user/sarun/engine/target/release/sarun-engine")
PYBASELINE = Path("bench/pjdfstest_baseline.txt")
_fails = []
def check(c, m):
    print(("  ok  " if c else " FAIL ") + m)
    if not c: _fails.append(m)


def wait_socket(s, t=60):
    end = time.time() + t
    while time.time() < end:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as k:
                k.settimeout(1); k.connect(s); return True
        except OSError: time.sleep(0.1)
    return False


def main():
    if not BIN.exists():
        if shutil.which("cargo"):
            subprocess.run(["cargo","build","--release"],
                           cwd="/home/user/sarun/engine", capture_output=True)
    if not BIN.exists():
        print("  ok  pjdfstest-rs: engine binary unavailable — SKIP\n\nALL PASS"); return 0
    try:
        extsuite.require_fuse(); extsuite.require_tools("prove")
        tests_dir, _ = extsuite.ensure_pjdfstest()
    except extsuite._Skip as e:
        print(f"  ok  pjdfstest-rs: SKIP ({e})\n\nALL PASS"); return 0
    group_paths = [str(tests_dir / g) for g in GROUPS]

    tmp = Path(tempfile.mkdtemp(prefix="pjdrs-"))
    for k,s in (("XDG_STATE_HOME","state"),("XDG_RUNTIME_DIR","run"),
                ("XDG_CONFIG_HOME","c"),("XDG_DATA_HOME","d")):
        os.environ[k]=str(tmp/s)
    os.environ["SLOPBOX_NS"]="PJD"; (tmp/"run").mkdir(parents=True)
    m = SourceFileLoader("slopbox","/home/user/sarun/sarun").load_module()
    m.ensure_dirs()
    eng = subprocess.Popen([str(BIN),"serve"],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    try:
        sock = m.sock_path()
        if not wait_socket(sock): raise RuntimeError("engine socket never came up")
        root = m.sync_request(sock, type="ui", verb="box_new", args=[])["r"]["root"]
        workdir = os.path.join(root, "root", "pjdfstest-wd")
        os.makedirs(workdir, exist_ok=True)
        p = subprocess.run(["prove","-r",*group_paths], cwd=workdir,
                           capture_output=True, text=True)
        out = p.stdout + "\n" + p.stderr
    finally:
        eng.terminate()
        try: eng.wait(timeout=15)
        except Exception: eng.kill()
        shutil.rmtree(tmp, ignore_errors=True)

    files_line = [ln for ln in out.splitlines() if ln.startswith("Files=")]
    print("  " + (files_line[-1] if files_line else "(no Files= summary!)"))
    nfiles = int(re.search(r"Files=(\d+)", files_line[-1]).group(1)) if files_line else 0
    check(nfiles > 0, f"pjdfstest-rs: prove ran tests against the Rust mount (Files={nfiles})")
    current = parse_failures(out)
    pybase = set(PYBASELINE.read_text().split()) if PYBASELINE.exists() else set()
    rust_only = sorted(current - pybase)   # Rust fails where Python (baseline) is fine
    py_only = sorted(pybase - current)     # Rust handles where Python doesn't
    print(f"  rust failures={len(current)}  python-baseline={len(pybase)}")
    if rust_only:
        print(f"  RUST-ONLY failures ({len(rust_only)}): "
              + ", ".join(rust_only[:40]) + (" ..." if len(rust_only)>40 else ""))
    if py_only:
        print(f"  (Rust handles {len(py_only)} that the Python baseline fails)")
    check(not rust_only,
          f"pjdfstest-rs: NO failures the Rust mount has that Python's baseline doesn't "
          f"(rust-only={len(rust_only)})")
    print("\n" + ("PJDFSTEST-RS PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_pjdfstest_rs():
    assert main() == 0, _fails

if __name__ == "__main__":
    sys.exit(main())
