#!/usr/bin/env python3
"""Syscall-level CONTRACT test for the native in-process coreutil builtins.

The differential tests prove a builtin's *output* matches GNU on a given input.
They cannot prove the *contract* that makes it a real in-process builtin (and not
a fake one that secretly forks). There are no gates and no fork-to-the-box's-
binary fallback any more: each of the 14 (`cat head tail wc nl tac basename
dirname seq expr tr cut uniq sort`) runs uutils IN-PROCESS, unconditionally. This test
asserts, at the syscall level:

  1. IN-PROCESS         -> the util is NEVER `execve`'d (it ran inside the engine,
                           no fork);
  2. no fd trampling    -> NO `dup2`/`dup3` onto the process's fd 0/1/2;
  3. right fd + content -> with both std streams redirected, NO write() hits the
                           process's fd 1/2 (no logical-sink leak), and the sink
                           bytes match the GNU reference for normal inputs;
  4. logical stdin      -> a piped / `< file` builtin reads the pipe/file fd, never
                           the engine's real fd 0 (the data-corruption bug class);
  5. LOCALIZATION       -> running many distinct utils in ONE process renders every
                           util's own messages (no raw Fluent keys like
                           `tac-error-open-error`) with the correct program name
                           (`wc:` not `sarun:`) — the uucore-per-thread fix;
  6. exit codes         -> true/false/[/expr/… return the right status;
  7. cat splice          -> `splice(2)` still fires for a file source.

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

# Syscalls we care about. `execve` reveals fork+exec of a util; `dup2`/`dup3`
# reveal fd-table trampling; `splice` is cat's fast path; clone/fork would reveal
# an unexpected child.
TRACE = "execve,dup2,dup3,splice,clone,vfork,fork"

EXECVE_PROG = re.compile(r'execve\("([^"]*)"')
DUP_ON_STD = re.compile(r'\bdup3?\([0-9]+,\s*([012])\b')
WRITE_FD = re.compile(r'\bwrite\((\d+),')
READ_FD0 = re.compile(r'\bread\(0,')

# The 13 native in-process coreutil builtins.
UTILS = ["cat", "head", "tail", "wc", "nl", "tac", "basename", "dirname", "seq",
         "expr", "tr", "cut", "uniq", "sort", "mkdir", "rmdir"]


def _require():
    if not shutil.which("strace"):
        raise RuntimeError("strace not found on PATH")
    if not os.path.exists(BIN):
        raise RuntimeError(f"engine binary missing: {BIN} (run `make engine`)")


def run_trace(script, cwd, traceset=TRACE):
    """Run `BIN brush-sh -- sh -c <script>` under strace; return the trace text."""
    tf = tempfile.NamedTemporaryFile(prefix="ct_", suffix=".strace", delete=False)
    tf.close()
    try:
        subprocess.run(
            ["strace", "-f", "-qq", "-e", f"trace={traceset}", "-o", tf.name,
             BIN, "brush-sh", "--", "sh", "-c", script],
            cwd=cwd, capture_output=True, text=True, timeout=60,
        )
        with open(tf.name, encoding="utf-8", errors="replace") as fh:
            return fh.read()
    finally:
        os.unlink(tf.name)


def execve_basenames(trace):
    """Program basenames execve'd, EXCLUDING the engine binary itself."""
    return {os.path.basename(p) for p in EXECVE_PROG.findall(trace)
            if os.path.basename(p) != "sarun"}


def dup_onto_std_count(trace):
    return len(DUP_ON_STD.findall(trace))


def writes_to_std(trace):
    return sum(1 for fd in WRITE_FD.findall(trace) if fd in ("1", "2"))


def run_redirected(script, cwd):
    """Run `<script> >OUT 2>ERR` under strace; return (trace, out_bytes, err_bytes).
    With both std streams redirected to FILES, the box's logical sinks are fds
    OTHER than 0/1/2 — so any write() to fd 1/2 is a process-global leak."""
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
    """The host (GNU coreutils) reference for `script`: stdout/stderr bytes."""
    p = subprocess.run(["sh", "-c", script], cwd=cwd, capture_output=True, timeout=60)
    return p.stdout, p.stderr


def box_run(script, cwd):
    """Run the box command, return (stdout+stderr text, exit code)."""
    p = subprocess.run([BIN, "brush-sh", "--", "sh", "-c", script],
                       cwd=cwd, capture_output=True, text=True, timeout=60)
    return p.stdout + p.stderr, p.returncode


# ── fixtures ─────────────────────────────────────────────────────────────────
def _setup(cwd):
    with open(os.path.join(cwd, "v.txt"), "w") as fh:
        fh.write("one\ntwo\nthree\nfour\n")
    with open(os.path.join(cwd, "s.txt"), "w") as fh:
        fh.write("b\na\nb\nc\na\n")
    os.mkdir(os.path.join(cwd, "sub"))


# ── 1+2+3: each builtin runs in-process, no fd trampling, content == GNU ──────
# (label, script) — normal inputs whose uutils output equals GNU.
INPROC = [
    ("cat",      "cat v.txt"),
    ("head",     "head -n2 v.txt"),
    ("tail",     "tail -n2 v.txt"),
    ("wc",       "wc -l v.txt"),
    ("nl",       "nl v.txt"),
    ("tac",      "tac v.txt"),
    ("basename", "basename /a/b.c .c"),
    ("dirname",  "dirname /a/b/c"),
    ("seq",      "seq 1 5"),
    ("expr",     "expr 2 + 3"),
    ("expr +1=1","expr +1 = 1"),
    ("expr substr ovf", "expr substr abcdef 1 99999999999999999999"),
    ("tr",       "printf abc | tr a-z A-Z"),
    ("cut",      "printf 'a:b:c\\n' | cut -d: -f2"),
    ("uniq",     "printf 'a\\na\\nb\\n' | uniq"),
    ("sort",     "sort s.txt"),
    # cp is a file-op builtin: its contract is "no forked /usr/bin/cp". The
    # relative operands resolve against the shell's logical cwd (the cp port
    # rewrites them), so a bare `cp a b` here copies within the box cwd.
    ("cp",       "cp v.txt vc.txt"),
    ("cp cwd",   "cd sub && cp ../v.txt out.txt"),
    # mkdir is a file-op builtin like cp: its relative operands resolve against
    # the shell's logical cwd. `-p` keeps these idempotent for the box-vs-GNU
    # differential (the box run and the GNU reference run the same script).
    ("mkdir",    "mkdir -p md_a"),
    ("mkdir cwd","cd sub && mkdir -p md_b && [ -d md_b ]"),
    # rmdir is a file-op builtin like cp/mkdir. Self-contained (create+remove) so
    # the box run and the GNU reference run leave no residue and produce no output.
    ("rmdir",    "mkdir -p rd_a && rmdir rd_a"),
    ("rmdir cwd","cd sub && mkdir -p rd_b && rmdir rd_b && [ ! -d rd_b ]"),
    # multi-stage all-builtin pipelines stay fully in-process
    ("sort|uniq -c", "sort s.txt | uniq -c"),
    ("tac|head",     "tac v.txt | head -n1"),
    ("echo|cat|wc",  "echo hi | cat | wc -c"),
]


def check_inproc(label, script, cwd):
    """No execve of any util (fully in-process) and no dup2/dup3 onto fd 0/1/2."""
    trace = run_trace(script, cwd)
    problems = []
    execd = execve_basenames(trace)
    if execd:
        problems.append(f"unexpected execve(s) {sorted(execd)} (must be in-process)")
    if dup_onto_std_count(trace):
        problems.append(f"{dup_onto_std_count(trace)} dup2/dup3 onto fd 0/1/2")
    return problems


def check_io(label, script, cwd):
    """Redirected: NO write() to process fd 1/2 (no leak), and sink == GNU."""
    trace, out, err = run_redirected(script, cwd)
    g_out, g_err = gnu_ref(script, cwd)
    problems = []
    if writes_to_std(trace):
        problems.append(f"{writes_to_std(trace)} write() to process fd 1/2 (leak)")
    if out != g_out:
        problems.append(f"stdout != GNU: box={out!r} gnu={g_out!r}")
    if err != g_err:
        problems.append(f"stderr != GNU: box={err!r} gnu={g_err!r}")
    return problems


# ── 4: logical stdin — a piped / `< file` builtin never reads the engine's fd 0 ─
NO_FD0 = [
    ("printf|head", 'printf "a\\nb\\nc\\n" | head -n1'),
    ("printf|tail", 'printf "a\\nb\\nc\\n" | tail -n1'),
    ("printf|wc",   "printf abc | wc -c"),
    ("printf|tac",  'printf "a\\nb\\n" | tac'),
    ("printf|nl",   'printf "a\\nb\\n" | nl'),
    ("printf|tr",   "printf abc | tr a-z A-Z"),
    ("printf|cut",  "printf 'a:b:c\\n' | cut -d: -f2"),
    ("printf|uniq", "printf 'a\\na\\nb\\n' | uniq"),
    ("printf|sort", "printf 'b\\na\\n' | sort"),
    ("head < file", "head -n1 < v.txt"),
    ("tail < file", "tail -n1 < v.txt"),
    ("cat < file",  "cat < v.txt"),
]


def check_no_fd0(label, script, cwd):
    trace = run_trace(script, cwd, "read,execve")
    problems = []
    n = len(READ_FD0.findall(trace))
    if n:
        problems.append(f"{n} read() of the engine's fd 0 (logical-stdin leak)")
    if execve_basenames(trace):
        problems.append(f"unexpected execve(s) {sorted(execve_basenames(trace))}")
    return problems


# ── 5: localization — many utils in ONE process, every message renders ────────
# An error-triggering command per util (each writes a diagnostic to stderr).
ERR_CMDS = [
    "cat /nope", "head /nope", "tail /nope", "wc /nope", "nl /nopedir", "tac /nope",
    "basename", "dirname", "seq", "expr 1 +", "tr", "cut -f1 /nope",
    "uniq /nope", "sort /nope", "mkdir /nope/deep", "rmdir /nope/deep",
]
# A raw Fluent key looks like `tac-error-open-error` / `expr-error-missing-...`:
# a util name followed by `-` then lowercase. Rendered English messages never do.
RAW_KEY = re.compile(r'\b(' + "|".join(UTILS) + r')-[a-z]')


def check_localization_session(order_label, cmds, cwd):
    """Run all the error commands in ONE box process; assert every diagnostic is
    a rendered message (no raw Fluent keys) with the correct program name."""
    script = "; ".join(f"{c} 2>&1" for c in cmds)
    text, _ = box_run(script, cwd)
    problems = []
    keys = RAW_KEY.findall(text)
    if keys:
        problems.append(f"raw Fluent key(s) for: {sorted(set(keys))} — localization "
                        f"corrupted in a multi-util session\n--- output ---\n{text}")
    if re.search(r'(?m)^sarun:', text):
        problems.append(f"wrong program-name prefix 'sarun:' (should be the util)\n{text}")
    return problems


# ── 6: exit codes ─────────────────────────────────────────────────────────────
EXIT_CASES = [
    ("true", "true", 0), ("false", "false", 1),
    ("[ -f v.txt ]", "[ -f v.txt ]", 0), ("[ -f nope ]", "[ -f nope ]", 1),
    ("head missing", "head /nope", 1), ("tail missing", "tail /nope", 1),
    ("wc -l ok", "wc -l v.txt", 0),
    ("mkdir -p ok", "mkdir -p md_exit", 0), ("mkdir bad parent", "mkdir /nope/deep", 1),
    ("rmdir ok", "mkdir -p rd_exit && rmdir rd_exit", 0),
    ("rmdir missing", "rmdir /nope/deep", 1),
    ("expr 5", "expr 5", 0), ("expr 0", "expr 0", 1),
    ("expr 1=2", "expr 1 = 2", 1), ("expr 1=1", "expr 1 = 1", 0),
    # regression guards for the uu_expr fork patch (leading-+ and substr-overflow
    # now match GNU in-process — no gate fallback exists):
    ("expr +1=1 (leading+)", "expr +1 = 1", 1),
    ("expr +5+1 (non-int)",  "expr +5 + 1", 2),
    ("expr substr overflow", "expr substr abcdef 1 99999999999999999999", 0),
]


def check_exit(label, script, expected, cwd):
    _, rc = box_run(script, cwd)
    return [] if rc == expected else [f"exit {rc}, expected {expected}"]


# ── 7 + existing surface: brush builtins, find/xargs/env, splice ──────────────
PURE = [
    ("echo (builtin)",        "echo hello"),
    ("printf (builtin)",      "printf '%s-%d\\n' a 5"),
    ("pwd (builtin)",         "pwd"),
    (": (builtin)",           ":"),
    ("[ test (builtin)",      "[ -f v.txt ] && echo yes"),
    ("export+printenv",       "export X=42; printenv X"),
    ("cd sub + pwd",          "cd sub && pwd"),
    ("find (in-process)",     "find . -maxdepth 1 -type f -name v.txt"),
    ("find -exec cat",        "find . -name v.txt -exec cat {} ';'"),
    ("xargs cat",             "printf v.txt | xargs cat"),
    ("env A=1 echo",          "env A=1 echo hi"),
    ("env A=1 printenv",      "env A=1 printenv A"),
    ("nice <builtin>",        "nice -n 5 cat v.txt"),
    ("setsid <builtin>",      "setsid cat v.txt"),
    ("nohup <builtin>",       "nohup cat v.txt"),
]


def check_pure(label, script, cwd):
    trace = run_trace(script, cwd)
    problems = []
    if execve_basenames(trace):
        problems.append(f"unexpected execve(s) {sorted(execve_basenames(trace))}")
    if dup_onto_std_count(trace):
        problems.append(f"{dup_onto_std_count(trace)} dup2/dup3 onto fd 0/1/2")
    return problems


def check_cat_splice(cwd):
    big = os.path.join(cwd, "big.txt")
    with open(big, "w") as fh:
        fh.write("x" * (256 * 1024))
    trace = run_trace("cat big.txt | cat > /dev/null", cwd)
    return [] if "splice(" in trace else ["cat did not use splice() (fast path lost)"]


def run_all():
    _require()
    with tempfile.TemporaryDirectory() as cwd:
        _setup(cwd)
        results = []
        for label, script in INPROC:
            results.append((f"{label} [in-process]", check_inproc(label, script, cwd)))
        for label, script in INPROC:
            results.append((f"{label} [io: fd+content]", check_io(label, script, cwd)))
        for label, script in NO_FD0:
            results.append((f"{label} [no fd0 read]", check_no_fd0(label, script, cwd)))
        results.append(("localization: forward session",
                        check_localization_session("fwd", ERR_CMDS, cwd)))
        results.append(("localization: reverse session",
                        check_localization_session("rev", list(reversed(ERR_CMDS)), cwd)))
        for label, script, code in EXIT_CASES:
            results.append((f"{label} [exit={code}]", check_exit(label, script, code, cwd)))
        for label, script in PURE:
            results.append((f"{label} [pure in-proc]", check_pure(label, script, cwd)))
        results.append(("cat splice fast path", check_cat_splice(cwd)))
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


def test_builtin_contract():
    results = run_all()
    bad = [(l, p) for l, p in results if p]
    assert not bad, "contract violations:\n" + "\n".join(f"{l}: {p}" for l, p in bad)


if __name__ == "__main__":
    res = run_all()
    n = _emit(res)
    print()
    if n:
        print(f"CONTRACT FAIL ({n} case(s))")
        sys.exit(1)
    print(f"CONTRACT PASS ({len(res)} cases)")
