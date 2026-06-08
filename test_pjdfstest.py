#!/usr/bin/env python3
"""pjdfstest POSIX-semantics battery against the live FUSE overlay, used as a
REGRESSION GATE rather than a pass/fail oracle.

The overlay diverges from a plain POSIX fs on purpose — uid/gid squashing, no
xattr (setxattr -> ENOSYS), synthetic dir/symlink inodes that don't persist
atime, virtual CA-bundle files — so pjdfstest can't pass clean. Instead we pin
the suite to a fixed revision, record the per-assertion failure set as a checked
in baseline (bench/pjdfstest_baseline.txt), and FAIL only when a failure appears
that is NOT in the baseline (a real regression). Assertions that newly PASS are
reported as "fixed" and never fail the gate; run with SARUN_PJDFSTEST_UPDATE=1 to
rewrite the baseline (e.g. after intentionally changing behavior).

    /home/user/venv/bin/python test_pjdfstest.py     # or: pytest test_pjdfstest.py

The baseline is anchored to this environment's kernel + the pinned pjdfstest
revision; on a very different kernel a few signatures may shift.
"""
import os
import re
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent / "bench"))
import extsuite

# Curated groups: the metadata ops the overlay implements, skipping BSD-only
# (chflags) and the heaviest uid-churn (chown) groups.
GROUPS = ["open", "truncate", "ftruncate", "unlink", "mkdir", "rmdir",
          "rename", "symlink", "link", "utimensat", "chmod", "mkfifo"]
BASELINE = Path(__file__).resolve().parent / "bench" / "pjdfstest_baseline.txt"

_fails = []


def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond:
        _fails.append(msg)


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


def test_pjdfstest_no_regressions():
    if not extsuite.fuse_available():
        return extsuite.skip("no /dev/fuse or fusermount3")
    if not extsuite.have("prove"):
        return extsuite.skip("prove (perl Test::Harness) not installed")
    tests_dir, _binary = extsuite.ensure_pjdfstest()
    group_paths = [str(tests_dir / g) for g in GROUPS]

    with extsuite.overlay_session() as box_root:
        workdir = os.path.join(box_root, "root", "pjdfstest-wd")
        os.makedirs(workdir, exist_ok=True)
        p = subprocess.run(["prove", "-r", *group_paths], cwd=workdir,
                           capture_output=True, text=True)
    out = p.stdout + "\n" + p.stderr
    files_line = [ln for ln in out.splitlines() if ln.startswith("Files=")]
    print("  " + (files_line[-1] if files_line else "(no Files= summary!)"))
    # Sanity: prove must actually have discovered and run tests.
    nfiles = int(re.search(r"Files=(\d+)", files_line[-1]).group(1)) if files_line else 0
    check(nfiles > 0, f"prove ran tests (Files={nfiles})")

    current = parse_failures(out)

    if os.environ.get("SARUN_PJDFSTEST_UPDATE") == "1":
        BASELINE.write_text("\n".join(sorted(current)) + "\n")
        print(f"  baseline rewritten: {len(current)} signatures -> {BASELINE}")
        return

    if not BASELINE.exists():
        check(False, f"baseline missing ({BASELINE}); run with "
                     f"SARUN_PJDFSTEST_UPDATE=1 to create it")
        return
    baseline = set(BASELINE.read_text().split())

    regressions = sorted(current - baseline)
    fixed = sorted(baseline - current)
    if fixed:
        print(f"  note: {len(fixed)} assertion(s) now PASS that the baseline "
              f"expects to fail (e.g. {', '.join(fixed[:5])}). "
              f"Re-baseline with SARUN_PJDFSTEST_UPDATE=1.")
    if regressions:
        print(f"  NEW failures ({len(regressions)}): {', '.join(regressions[:20])}"
              + (" ..." if len(regressions) > 20 else ""))
    check(not regressions,
          f"no new pjdfstest failures vs baseline "
          f"(current={len(current)}, baseline={len(baseline)})")


if __name__ == "__main__":
    try:
        test_pjdfstest_no_regressions()
    except extsuite._Skip:
        sys.exit(0)
    except Exception:
        import traceback
        traceback.print_exc()
        _fails.append("exception")
    print("\n" + ("ALL PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    sys.exit(1 if _fails else 0)
