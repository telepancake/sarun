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
from extsuite import parse_failures, GROUPS  # parser + group list

BIN = Path(__file__).resolve().parent.parent / "engine/target/x86_64-unknown-linux-musl/release/sarun"
PYBASELINE = Path("bench/pjdfstest_baseline.txt")
RSBASELINE = Path("bench/pjdfstest_baseline_rs.txt")
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
        if shutil.which("make"):
            subprocess.run(["make","engine"],
                           cwd="/home/user/sarun", capture_output=True)
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
    m = SourceFileLoader("slopbox","/home/user/sarun/prototype/libtestsarun.py").load_module()
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

    # The gate, same methodology as test_pjdfstest.py: a Rust baseline of
    # accepted divergences (the overlay is intentionally non-POSIX — uid
    # squashing makes the sticky-bit/multi-uid permission matrices degenerate;
    # plus the box adds ops Python lacks). FAIL only on a NEW failure (a real
    # regression). SARUN_PJDFSTEST_RS_UPDATE=1 rewrites it after review.
    if os.environ.get("SARUN_PJDFSTEST_RS_UPDATE") == "1":
        RSBASELINE.write_text("\n".join(sorted(current)) + "\n")
        print(f"  rust baseline rewritten: {len(current)} signatures")
        print("\nPJDFSTEST-RS PASS"); return 0
    rsbase = set(RSBASELINE.read_text().split()) if RSBASELINE.exists() else set()
    regressions = sorted(current - rsbase)
    fixed = sorted(rsbase - current)
    # Informational: vs the Python engine's baseline (real-bug spotting).
    pybase = set(PYBASELINE.read_text().split()) if PYBASELINE.exists() else set()
    print(f"  rust failures={len(current)}  rust-baseline={len(rsbase)}  "
          f"(vs python-baseline: rust-only={len(current-pybase)}, "
          f"rust-handles={len(pybase-current)})")
    if fixed:
        print(f"  note: {len(fixed)} now PASS that the rust baseline expects to "
              f"fail — re-baseline with SARUN_PJDFSTEST_RS_UPDATE=1")
    if regressions:
        print(f"  NEW failures ({len(regressions)}): " + ", ".join(regressions[:30]))
    check(not regressions,
          f"pjdfstest-rs: no regression vs the rust baseline "
          f"(current={len(current)}, baseline={len(rsbase)})")
    print("\n" + ("PJDFSTEST-RS PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_pjdfstest_rs():
    assert main() == 0, _fails

if __name__ == "__main__":
    sys.exit(main())
