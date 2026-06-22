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
# read(0, ... — a read of the engine's REAL fd 0. A piped-stdin builtin must read
# the pipe fd, never fd 0 (reading fd 0 steals bytes from whatever owns the
# engine's stdin — the find -files0-from / xargs data-corruption bug class).
READ_FD0 = re.compile(r'\bread\(0,')


def run_trace_set(script, cwd, traceset):
    """Run the box command under strace tracing `traceset`; return (trace, rc)."""
    tf = tempfile.NamedTemporaryFile(prefix="ts_", suffix=".strace", delete=False)
    tf.close()
    try:
        p = subprocess.run(
            ["strace", "-f", "-qq", "-e", f"trace={traceset}", "-o", tf.name,
             BIN, "brush-sh", "--", "sh", "-c", script],
            cwd=cwd, capture_output=True, timeout=60,
        )
        with open(tf.name, encoding="utf-8", errors="replace") as fh:
            return fh.read(), p.returncode
    finally:
        os.unlink(tf.name)


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
    os.mkdir(os.path.join(cwd, "sub"))  # for the logical-cwd `cd sub` case


# Piped-stdin cases: the builtin must read the PIPE, never the engine's real
# fd 0, and stay in-process. (label, script)
NO_FD0_CASES = [
    ("printf|head -n1",   'printf "a\\nb\\nc\\n" | head -n1'),
    ("printf|wc -c",      "printf abc | wc -c"),
    ("echo|cat|wc -c",    "echo hi | cat | wc -c"),
    ("printf|tac",        'printf "a\\nb\\n" | tac'),
    ("printf|nl",         'printf "a\\nb\\n" | nl'),
]


def check_no_fd0(label, script, cwd):
    """A piped-stdin builtin reads the pipe, NEVER the engine's fd 0, and runs
    fully in-process (no execve)."""
    trace, _ = run_trace_set(script, cwd, "read,execve")
    problems = []
    n = len(READ_FD0.findall(trace))
    if n:
        problems.append(f"{n} read() of the engine's fd 0 (logical-stdin leak)")
    execd = execve_basenames(trace)
    if execd:
        problems.append(f"unexpected execve(s) {sorted(execd)}")
    return problems


# Exit-code correctness for in-process builtins. (label, script, expected_code)
EXIT_CASES = [
    ("true",            "true",              0),
    ("false",           "false",             1),
    ("[ -f v.txt ]",    "[ -f v.txt ]",      0),
    ("[ -f nope ]",     "[ -f nope ]",       1),
    ("head missing rc", "head nope.txt",     1),
    ("wc -l rc",        "wc -l v.txt",       0),
]


def check_exit(label, script, expected, cwd):
    p = subprocess.run([BIN, "brush-sh", "--", "sh", "-c", script],
                       cwd=cwd, capture_output=True, timeout=60)
    if p.returncode != expected:
        return [f"exit {p.returncode}, expected {expected}"]
    return []


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


# EXISTING in-process surface (shell builtins + find/xargs/env exec-wrappers).
# These must run FULLY in-process: the engine is the ONLY thing that ever
# execve's (execve_basenames empty), and nothing dup2's onto fd 0/1/2. This
# covers the "runs other builtins/scripts in-process" half of the contract:
# `find -exec cat`, `xargs cat`, and `env A=1 echo` each dispatch their
# sub-command THROUGH brush (the cat/echo builtin), so no binary is forked.
PURE_CASES = [
    ("echo (shell builtin)",          "echo hello"),
    ("printf (shell builtin)",        "printf '%s-%d\\n' a 5"),
    ("pwd (shell builtin)",           "pwd"),
    ("true (shell builtin)",          "true"),
    ("find (in-process)",             "find . -maxdepth 1 -type f -name v.txt"),
    ("find -exec cat (sub via brush)", "find . -name v.txt -exec cat {} ';'"),
    ("xargs cat (sub via brush)",     "printf v.txt | xargs cat"),
    ("env A=1 echo (sub via brush)",  "env A=1 echo hi"),
    ("env A=1 printenv (sub via brush)", "env A=1 printenv A"),
    (": (no-op builtin)",             ":"),
    ("[ test builtin )",              "[ -f v.txt ] && echo yes"),
    ("export + printenv (in-proc)",   "export X=42; printenv X"),
    ("cd sub + pwd (logical cwd)",    "cd sub && pwd"),
    ("echo|cat|wc 3-stage (in-proc)", "echo hi | cat | wc -c"),
]


def check_pure(label, script, cwd):
    """A fully in-process command: NOTHING but the engine binary is execve'd, and
    no dup2/dup3 onto fd 0/1/2. Proves the sub-command (cat/echo/printenv) ran as
    an in-process builtin via brush, not a forked binary."""
    _, trace = run_trace(script, cwd)
    problems = []
    execd = execve_basenames(trace)
    if execd:
        problems.append(f"unexpected execve(s) {sorted(execd)} (expected fully "
                        f"in-process)")
    if dup_onto_std_count(trace):
        problems.append(f"{dup_onto_std_count(trace)} dup2/dup3 onto fd 0/1/2")
    return problems


# Content+destination for the existing in-process surface (skip pwd/true: pwd's
# logical-vs-physical path can differ on symlinked tmp, true has no output).
CASES_IO_EXISTING = [
    ("echo",                "echo hello"),
    ("printf",              "printf '%s-%d\\n' a 5"),
    ("find single",         "find . -maxdepth 1 -type f -name v.txt"),
    ("find -exec cat",      "find . -name v.txt -exec cat {} ';'"),
    ("xargs cat",           "printf v.txt | xargs cat"),
    ("env A=1 echo",        "env A=1 echo hi"),
    ("env A=1 printenv A",  "env A=1 printenv A"),
    ("echo|cat|wc -c",      "echo hi | cat | wc -c"),
    ("export + printenv",   "export X=42; printenv X"),
]


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
        # Existing in-process surface: shell builtins + find/xargs/env.
        for label, script in PURE_CASES:
            results.append((f"{label} [pure in-proc]", check_pure(label, script, cwd)))
        for label, script in CASES_IO_EXISTING:
            results.append((f"{label} [io: fd+content]", check_io(label, script, cwd)))
        # Logical-stdin: a piped builtin must never read the engine's fd 0.
        for label, script in NO_FD0_CASES:
            results.append((f"{label} [no fd0 read]", check_no_fd0(label, script, cwd)))
        # Exit-code correctness.
        for label, script, code in EXIT_CASES:
            results.append((f"{label} [exit={code}]", check_exit(label, script, code, cwd)))
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
