#!/usr/bin/env python3
"""Syscall-level CONTRACT test for the native injected-I/O brush builtins.

The differential tests prove a builtin's *output* matches GNU. They cannot prove
the *contract* that makes it a "proper builtin":

  1. parallel-safe / runs IN-PROCESS  -> the util is NOT `execve`'d when its argv
     passes the gate (it really ran inside the engine, no fork);
  2. no global-state trampling        -> NO `dup2`/`dup3` onto the process's
     fd 0/1/2 around the call;
  3. correct with EXTERNAL processes   -> a gate-REFUSED argv DOES `execve` the
     host binary (fork+exec fallback);
  4. cat keeps its splice(2) fast path -> `splice` fires for a file source.

Those are syscall-level facts, so we assert them by running each case under
`strace` and parsing the trace. This is the programmatic version of the manual
`strace` check the porting story describes.

Standalone:  engine/test_builtin_contract.py        (prints CONTRACT PASS/FAIL)
pytest:      uv run --with pytest pytest engine/test_builtin_contract.py

Requires `strace` (ptrace) and GNU coreutils on PATH, plus the built engine at
engine/target/x86_64-unknown-linux-musl/release/sarun (run `make engine` first).
"""

import os
import re
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
BIN = os.path.join(HERE, "target/x86_64-unknown-linux-musl/release/sarun")

# Syscalls we care about. `clone`/`vfork`/`fork` would reveal an unexpected
# child; `execve` reveals fork+exec of a util; `dup2`/`dup3` reveal fd-table
# trampling; `splice` is cat's fast path.
TRACE = "execve,dup2,dup3,splice,clone,vfork,fork"

EXECVE_PROG = re.compile(r'execve\("([^"]*)"')
# dup2(old, new) / dup3(old, new, flags): new == 0/1/2 is a std-fd redirect.
DUP_ON_STD = re.compile(r'\bdup3?\([0-9]+,\s*([012])\b')


def _require():
    if not shutil.which("strace"):
        raise RuntimeError("strace not found on PATH")
    if not os.path.exists(BIN):
        raise RuntimeError(f"engine binary missing: {BIN} (run `make engine`)")


def run_trace(script, cwd):
    """Run `BIN brush-sh -- sh -c <script>` under strace; return (out, trace)."""
    tf = tempfile.NamedTemporaryFile(prefix="contract_", suffix=".strace",
                                     delete=False)
    tf.close()
    try:
        proc = subprocess.run(
            ["strace", "-f", "-qq", "-e", f"trace={TRACE}", "-o", tf.name,
             BIN, "brush-sh", "--", "sh", "-c", script],
            cwd=cwd, capture_output=True, text=True, timeout=60,
        )
        with open(tf.name, encoding="utf-8", errors="replace") as fh:
            trace = fh.read()
        return proc.stdout, trace
    finally:
        os.unlink(tf.name)


def execve_basenames(trace):
    """Program basenames execve'd, EXCLUDING the engine binary itself."""
    names = set()
    for prog in EXECVE_PROG.findall(trace):
        base = os.path.basename(prog)
        if base != "sarun":
            names.add(base)
    return names


def dup_onto_std_count(trace):
    return len(DUP_ON_STD.findall(trace))


# write(FD, ... — the fd a write() targeted.
WRITE_FD = re.compile(r'\bwrite\((\d+),')


def run_redirected(script, cwd):
    """Run `<script> >OUT 2>ERR` under strace -e trace=write; return
    (trace, out_bytes, err_bytes). With both std streams redirected to FILES, the
    box's logical sinks are fds OTHER than the process's 0/1/2 — so any write() the
    trace shows to fd 1 or fd 2 is a process-global leak (the bug class the
    differential tests, which capture process stdout/stderr, cannot see)."""
    o = tempfile.NamedTemporaryFile(prefix="bo_", dir=cwd, delete=False); o.close()
    e = tempfile.NamedTemporaryFile(prefix="be_", dir=cwd, delete=False); e.close()
    tf = tempfile.NamedTemporaryFile(prefix="wtr_", suffix=".strace", delete=False)
    tf.close()
    try:
        full = f"{script} >{o.name} 2>{e.name}"
        subprocess.run(
            ["strace", "-f", "-qq", "-e", "trace=write", "-s", "65536",
             "-o", tf.name, BIN, "brush-sh", "--", "sh", "-c", full],
            cwd=cwd, capture_output=True, timeout=60,
        )
        with open(tf.name, encoding="utf-8", errors="replace") as fh:
            trace = fh.read()
        with open(o.name, "rb") as fh:
            out = fh.read()
        with open(e.name, "rb") as fh:
            err = fh.read()
        return trace, out, err
    finally:
        for p in (o.name, e.name, tf.name):
            os.unlink(p)


def gnu_ref(script, cwd):
    """The host (GNU coreutils) reference for `script`: its stdout/stderr bytes."""
    p = subprocess.run(["sh", "-c", script], cwd=cwd, capture_output=True,
                       timeout=60)
    return p.stdout, p.stderr


def writes_to_std(trace):
    """Count write() syscalls in the trace that targeted process fd 1 or 2."""
    return sum(1 for fd in WRITE_FD.findall(trace) if fd in ("1", "2"))


# (label, script, util, mode) — mode "inproc" or "external".
CASES = [
    # In-process: gate accepts -> util runs inside the engine.
    ("cat file",      "cat v.txt",            "cat",  "inproc"),
    ("head -n2",      "head -n2 v.txt",       "head", "inproc"),
    ("head -c5 pipe", "printf abcdefgh | head -c5", "head", "inproc"),
    ("wc -l",         "wc -l v.txt",          "wc",   "inproc"),
    ("wc -c pipe",    "printf abc | wc -c",   "wc",   "inproc"),
    ("nl file",       "nl v.txt",             "nl",   "inproc"),
    ("tac file",      "tac v.txt",            "tac",  "inproc"),
    # Gate fallback: divergent argv -> fork+exec the host binary.
    ("wc -w (locale)",   "printf 'a b c\\n' | wc -w", "wc",   "external"),
    ("head --version",   "head --version",            "head", "external"),
    ("tac --help",       "tac --help",                "tac",  "external"),
    ("nl -s:: (multi)",  "nl -s :: v.txt",            "nl",   "external"),
    ("head -n0b (suffix)", "head -n0b v.txt",         "head", "external"),
]


def _setup(cwd):
    with open(os.path.join(cwd, "v.txt"), "w") as fh:
        fh.write("one\ntwo\nthree\nfour\n")


def check_case(label, script, util, mode, cwd):
    _, trace = run_trace(script, cwd)
    execd = execve_basenames(trace)
    dups = dup_onto_std_count(trace)
    problems = []
    if mode == "inproc":
        if util in execd:
            problems.append(f"{util} was execve'd (expected in-process); "
                            f"execve set={sorted(execd)}")
        if dups:
            problems.append(f"{dups} dup2/dup3 onto fd 0/1/2 (expected 0)")
    else:  # external
        if util not in execd:
            problems.append(f"{util} was NOT execve'd (expected host fallback); "
                            f"execve set={sorted(execd)}")
    return problems


def check_pipeline_inprocess(cwd):
    """tac | head: a two-stage in-process pipeline forks NEITHER util."""
    _, trace = run_trace("tac v.txt | head -n1", cwd)
    execd = execve_basenames(trace)
    bad = {u for u in ("tac", "head") if u in execd}
    if bad:
        return [f"pipeline tac|head execve'd {sorted(bad)} (expected both in-process)"]
    return []


def check_cat_splice(cwd):
    """cat keeps its splice(2) fast path for a real file source into a pipe."""
    big = os.path.join(cwd, "big.txt")
    with open(big, "w") as fh:
        fh.write("x" * (256 * 1024))
    _, trace = run_trace("cat big.txt | cat > /dev/null", cwd)
    if "splice(" not in trace:
        return ["cat did not use splice() for a file source (fast path lost)"]
    return []


# In-process argvs to check for content+destination via strace write capture.
# (label, script). stdout AND stderr are redirected to files by run_redirected,
# so a proper builtin writes ONLY the logical-sink fds — never process fd 1/2.
CASES_IO = [
    ("head -n2",        "head -n2 v.txt"),
    ("head -c5 pipe",   "printf abcdefgh | head -c5"),
    ("head missing",    "head nope.txt"),          # diagnostic must hit logical err
    ("wc -lc",          "wc -lc v.txt"),
    ("nl file",         "nl v.txt"),
    ("tac file",        "tac v.txt"),
    ("cat file",        "cat v.txt"),
    ("tac|head pipe",   "tac v.txt | head -n1"),
]


def check_io(label, script, cwd):
    """strace write-capture: with stdout/stderr redirected to files, assert the
    builtin makes NO write() to process fd 1/2 (no global-state leak), and the
    sink files match the GNU reference byte-for-byte (right content, right fd)."""
    trace, out, err = run_redirected(script, cwd)
    g_out, g_err = gnu_ref(script, cwd)
    problems = []
    leaks = writes_to_std(trace)
    if leaks:
        problems.append(f"{leaks} write() to process fd 1/2 with both streams "
                        f"redirected (logical-sink leak)")
    if out != g_out:
        problems.append(f"stdout != GNU: box={out!r} gnu={g_out!r}")
    if err != g_err:
        problems.append(f"stderr != GNU: box={err!r} gnu={g_err!r}")
    return problems


def run_all():
    _require()
    with tempfile.TemporaryDirectory() as cwd:
        _setup(cwd)
        results = []
        for label, script, util, mode in CASES:
            probs = check_case(label, script, util, mode, cwd)
            results.append((f"{label} [{mode}]", probs))
        results.append(("pipeline tac|head [inproc]", check_pipeline_inprocess(cwd)))
        results.append(("cat splice fast path", check_cat_splice(cwd)))
        for label, script in CASES_IO:
            results.append((f"{label} [io: fd+content]", check_io(label, script, cwd)))
    return results


def _emit(results):
    failed = 0
    for label, probs in results:
        if probs:
            failed += 1
            print(f"FAIL  {label}")
            for p in probs:
                print(f"        - {p}")
        else:
            print(f"ok    {label}")
    return failed


# ── pytest entry points ──────────────────────────────────────────────────────
def test_builtin_syscall_contract():
    results = run_all()
    failed = [(l, p) for l, p in results if p]
    assert not failed, "contract violations:\n" + "\n".join(
        f"{l}: {p}" for l, p in failed)


if __name__ == "__main__":
    res = run_all()
    n_fail = _emit(res)
    print()
    if n_fail:
        print(f"CONTRACT FAIL ({n_fail} case(s))")
        sys.exit(1)
    print(f"CONTRACT PASS ({len(res)} cases)")
