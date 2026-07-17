#!/usr/bin/env python3
"""Syscall-level contract test for the in-process coreutil builtins.

Every util in UTILS runs uutils IN-PROCESS, unconditionally (no gate, no fork
fallback). Asserts at the syscall level:

  1. IN-PROCESS         -> util is NEVER execve'd (runs inside the engine)
  2. no fd trampling    -> no dup2/dup3 onto fd 0/1/2
  3. right fd + content -> with std streams redirected to files, no write() to
                           fd 1/2 (logical-sink leak), and bytes match GNU
  4. logical stdin      -> piped / `< file` builtin reads the pipe/file fd, never
                           the engine's fd 0 (data-corruption bug class)
  5. localization       -> many utils in ONE process render their own messages
                           (no raw Fluent keys like `tac-error-open-error`, correct
                           program name `wc:` not `sarun:`) — uucore-per-thread fix
  6. exit codes         -> true/false/[/expr/… return the right status
  7. cat splice         -> splice(2) still fires for a file source

Standalone:  engine/test_builtin_contract.py        (prints CONTRACT PASS/FAIL)
pytest:      uv run --with pytest pytest engine/test_builtin_contract.py

Requires `strace` (ptrace) and GNU coreutils on PATH, plus the built static
engine for the current host architecture (run `make engine` first).
"""

import os
import platform
import re
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
_arch = {"arm64": "aarch64", "amd64": "x86_64"}.get(
    platform.machine().lower(), platform.machine().lower()
)
_target = os.environ.get("SARUN_ENGINE_TARGET", f"{_arch}-unknown-linux-musl")
BIN = os.path.join(HERE, "target", _target, "release", "sarun")

# execve: fork+exec of a util; dup2/dup3: fd-table trampling; splice: cat fast path.
TRACE = "execve,dup2,dup3,splice,clone,vfork,fork"

EXECVE_PROG = re.compile(r'execve\("([^"]*)"')
DUP_ON_STD = re.compile(r'\bdup3?\([0-9]+,\s*([012])\b')
WRITE_FD = re.compile(r'\bwrite\((\d+),')
READ_FD0 = re.compile(r'\bread\(0,')

# All native in-process coreutil builtins under test.
UTILS = ["cat", "head", "tail", "wc", "nl", "tac", "basename", "dirname", "seq",
         "expr", "tr", "cut", "uniq", "sort", "mkdir", "rmdir", "rm", "mv", "ln",
         "touch", "readlink", "realpath", "mktemp", "tee", "chmod", "chown",
         "install", "uname", "nproc", "id", "whoami"]


def _require():
    if not shutil.which("strace"):
        raise RuntimeError("strace not found on PATH")
    if not os.path.exists(BIN):
        raise RuntimeError(f"engine binary missing: {BIN} (run `make engine`)")


def run_trace(script, cwd, traceset=TRACE):
    """Run script under strace; return the trace text."""
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
    """Basenames execve'd, excluding the engine binary itself."""
    return {os.path.basename(p) for p in EXECVE_PROG.findall(trace)
            if os.path.basename(p) != "sarun"}


def dup_onto_std_count(trace):
    return len(DUP_ON_STD.findall(trace))


def writes_to_std(trace):
    return sum(1 for fd in WRITE_FD.findall(trace) if fd in ("1", "2"))


def run_redirected(script, cwd):
    """Run script with stdout/stderr redirected to files; return (trace, out_bytes, err_bytes).
    Logical sinks land on fds > 2, so any write() to fd 1/2 is a process-global leak."""
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
    """Host (GNU coreutils) reference stdout/stderr bytes for `script`."""
    p = subprocess.run(["sh", "-c", script], cwd=cwd, capture_output=True, timeout=60)
    return p.stdout, p.stderr


def box_run(script, cwd):
    """Run script in a box; return (stdout+stderr text, exit code)."""
    p = subprocess.run([BIN, "brush-sh", "--", "sh", "-c", script],
                       cwd=cwd, capture_output=True, text=True, timeout=60)
    return p.stdout + p.stderr, p.returncode


# ── fixtures ─────────────────────────────────────────────────────────────────
def _setup(cwd):
    with open(os.path.join(cwd, "v.txt"), "w") as fh:
        fh.write("one\ntwo\nthree\nfour\n")
    with open(os.path.join(cwd, "s.txt"), "w") as fh:
        fh.write("b\na\nb\nc\na\n")
    # Known starting mode: `chmod 600` is a no-op on re-run, keeping differential stable.
    m = os.path.join(cwd, "m.txt")
    with open(m, "w") as fh:
        fh.write("x\n")
    os.chmod(m, 0o600)
    # symlink target is deterministic (what readlink prints)
    link = os.path.join(cwd, "lnk")
    if not os.path.lexists(link):
        os.symlink("v.txt", link)
    os.mkdir(os.path.join(cwd, "sub"))


# ── 1+2+3: in-process, no fd trampling, content == GNU ───────────────────────
# (label, script) — inputs whose uutils output equals GNU.
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
    # file-op builtins: relative operands resolve against the shell's logical cwd.
    ("cp",       "cp v.txt vc.txt"),
    ("cp cwd",   "cd sub && cp ../v.txt out.txt"),
    # -p: idempotent for the box-vs-GNU differential (both runs see same script).
    ("mkdir",    "mkdir -p md_a"),
    ("mkdir cwd","cd sub && mkdir -p md_b && [ -d md_b ]"),
    # self-contained create+remove: no residue, no output.
    ("rmdir",    "mkdir -p rd_a && rmdir rd_a"),
    ("rmdir cwd","cd sub && mkdir -p rd_b && rmdir rd_b && [ ! -d rd_b ]"),
    ("rm",       "printf x > rm_a && rm rm_a"),
    ("rm cwd",   "cd sub && printf x > rm_b && rm rm_b && [ ! -e rm_b ]"),
    # mv: relative operands AND -t resolve against logical cwd.
    ("mv",       "printf x > mv_a && mv mv_a mv_a2 && rm -f mv_a2"),
    ("mv cwd",   "cd sub && printf x > mv_b && mv mv_b mv_b2 && [ -e mv_b2 ] && rm -f mv_b2"),
    ("mv -t cwd","cd sub && printf x > mv_c && mkdir -p mv_d && mv -t mv_d mv_c && [ -e mv_d/mv_c ] && rm -rf mv_d"),
    # ln: relative operands AND -t resolve against logical cwd.
    ("ln -s",    "printf x > ln_a && ln -sf ln_a ln_a_l && [ -L ln_a_l ] && rm -f ln_a ln_a_l"),
    ("ln cwd",   "cd sub && printf x > ln_b && ln -sf ln_b ln_b_l && [ -L ln_b_l ] && rm -f ln_b ln_b_l"),
    ("touch",    "touch tch_a"),
    ("touch cwd","cd sub && touch tch_b && [ -e tch_b ]"),
    # path-op builtins: relative operands resolve against logical cwd.
    ("readlink", "readlink lnk"),
    ("readlink cwd", "cd sub && readlink ../lnk"),
    ("realpath", "realpath v.txt"),
    ("realpath cwd", "cd sub && realpath ../v.txt"),
    # tee copies stdin to stdout AND its file operand; check_io compares only stdout/stderr.
    ("tee",      "printf payload | tee teeout.txt"),
    ("tee cwd",  "cd sub && printf X | tee tee_rel.txt"),
    # chmod/chown/install: idempotent scripts so box and GNU reference match.
    ("chmod",    "chmod 600 m.txt"),
    ("chmod cwd","cd sub && chmod 700 . && chmod 755 ."),
    # chown --reference=F F is a no-op (no EPERM, no output, exit 0); no $() subshell
    # (cmdsubst-pipe write(1) would be misread as a fd-1 leak by the strace check).
    ("chown",    "chown --reference=m.txt m.txt"),
    ("chown cwd","cd sub && chown --reference=../m.txt ../m.txt"),
    ("install",  "install -m 600 v.txt iv.txt"),
    ("install -d","install -d id_a"),
    ("install cwd","cd sub && install -m 644 ../v.txt iout.txt"),
    # info utils: same process env as the GNU reference run → values match.
    ("uname -s", "uname -s"),
    ("nproc",    "nproc"),
    ("id -u",    "id -u"),
    ("id -un",   "id -un"),
    ("whoami",   "whoami"),
    # multi-stage all-builtin pipelines stay fully in-process
    ("sort|uniq -c", "sort s.txt | uniq -c"),
    ("tac|head",     "tac v.txt | head -n1"),
    ("echo|cat|wc",  "echo hi | cat | wc -c"),
    ("echo|tee|cat", "echo hi | tee teep.txt | cat"),
]

# In-process-only (not GNU-equality checked): mktemp output is random.
# Still asserts in-process execution, no fd trampling, and logical-cwd honor.
INPROC_ONLY = [
    ("mktemp",     "mktemp mt.XXXXXX"),
    ("mktemp cwd", "cd sub && mktemp mt.XXXXXX"),
]


def check_inproc(label, script, cwd):
    """Assert: no execve of any util and no dup2/dup3 onto fd 0/1/2."""
    trace = run_trace(script, cwd)
    problems = []
    execd = execve_basenames(trace)
    if execd:
        problems.append(f"unexpected execve(s) {sorted(execd)} (must be in-process)")
    if dup_onto_std_count(trace):
        problems.append(f"{dup_onto_std_count(trace)} dup2/dup3 onto fd 0/1/2")
    return problems


def check_io(label, script, cwd):
    """Assert: no write() to process fd 1/2, and sink bytes == GNU."""
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


# ── 4: logical stdin — piped / `< file` builtin never reads the engine's fd 0 ──
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
    # rm/mv/ln read logical stdin only for -i; a plain pipe must not touch fd 0.
    ("printf|rm",   "printf y | (printf x > rm_fd0 && rm rm_fd0)"),
    ("printf|mv",   "printf y | (printf x > mv_fd0 && mv mv_fd0 mv_fd0b && rm -f mv_fd0b)"),
    ("printf|ln",   "printf y | (printf x > ln_fd0 && ln -sf ln_fd0 ln_fd0l && rm -f ln_fd0 ln_fd0l)"),
    # tee reads its stdin; must read the pipe/file fd, never the engine's fd 0.
    ("printf|tee",  "printf abc | tee t_fd0.txt"),
    ("tee < file",  "tee t_fd0b.txt < v.txt"),
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
# One error-triggering command per util (each writes a diagnostic to stderr).
ERR_CMDS = [
    "cat /nope", "head /nope", "tail /nope", "wc /nope", "nl /nopedir", "tac /nope",
    "basename", "dirname", "seq", "expr 1 +", "tr", "cut -f1 /nope",
    "uniq /nope", "sort /nope", "mkdir /nope/deep", "rmdir /nope/deep",
    "rm /nope/deep", "mv /nope/deep /also/nope", "ln /nope/deep /also/nope",
    "touch /nope/deep",
    "readlink -v .", "realpath /no/such/deep/path",
    "mktemp /no/such/dir/fooXXXX", "tee /no/such/dir/f.txt",
    "chmod 600 /nope", "chown root /nope", "install /nope /also/nope",
    "id nosuchuser_zzz", "nproc --ignore=notanumber", "id -n",
]
# Raw Fluent key shape: `tac-error-open-error` — util name then `-` then lowercase.
RAW_KEY = re.compile(r'\b(' + "|".join(UTILS) + r')-[a-z]')


def check_localization_session(order_label, cmds, cwd):
    """Run all error commands in ONE process; assert rendered messages, correct program name."""
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
    # rmdir -p walks only the operand's ancestors (a/b, a), not the cwd's ancestors.
    # Regression: a cwd-joined operand previously walked up to / and failed.
    ("rmdir -p", "mkdir -p rdp/x/y && rmdir -p rdp/x/y && [ ! -d rdp ]", 0),
    ("rm ok", "printf x > rm_exit && rm rm_exit", 0),
    ("rm missing", "rm /nope/deep", 1), ("rm -f missing", "rm -f /nope/deep", 0),
    ("mv ok", "printf x > mv_exit && mv mv_exit mv_exit2 && rm -f mv_exit2", 0),
    ("mv missing", "mv /nope/deep /also/nope", 1),
    ("ln ok", "printf x > ln_exit && ln -s ln_exit ln_exit_l && rm -f ln_exit ln_exit_l", 0),
    ("ln missing", "ln /nope/deep /also/nope", 1),
    # POSIX: ln -s stores target verbatim; relative target must not be cwd-rewritten.
    ("ln -s relative target verbatim",
     "ln -sf tgt lnrel && [ \"$(readlink lnrel)\" = tgt ] && rm -f lnrel", 0),
    ("touch ok", "touch tch_exit", 0), ("touch bad dir", "touch /nope/deep", 1),
    ("expr 5", "expr 5", 0), ("expr 0", "expr 0", 1),
    ("expr 1=2", "expr 1 = 2", 1), ("expr 1=1", "expr 1 = 1", 0),
    # uu_expr patch: leading-+ and substr-overflow match GNU (no gate fallback exists).
    ("expr +1=1 (leading+)", "expr +1 = 1", 1),
    ("expr +5+1 (non-int)",  "expr +5 + 1", 2),
    ("expr substr overflow", "expr substr abcdef 1 99999999999999999999", 0),
    ("chmod ok", "chmod 600 m.txt", 0), ("chmod missing", "chmod 600 /nope", 1),
    ("chown self ok", "chown --reference=m.txt m.txt", 0), ("chown missing", "chown root /nope", 1),
    ("install ok", "install -m 600 v.txt iexit.txt", 0),
    ("install -d ok", "install -d id_exit", 0),
    ("install bad", "install /nope /also/nope", 1),
    ("uname -s ok", "uname -s", 0), ("nproc ok", "nproc", 0),
    ("id -u ok", "id -u", 0), ("whoami ok", "whoami", 0),
    ("id no such user", "id nosuchuser_zzz", 1),
]


def check_exit(label, script, expected, cwd):
    _, rc = box_run(script, cwd)
    return [] if rc == expected else [f"exit {rc}, expected {expected}"]


# ── 7 + existing surface: brush builtins, find/xargs/env ─────────────────────
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
        for label, script in INPROC_ONLY:
            results.append((f"{label} [in-process]", check_inproc(label, script, cwd)))
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
