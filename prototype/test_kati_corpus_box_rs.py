#!/usr/bin/env python3
"""The kati conformance corpus, run through a REAL sarun -b box.

The vendored corpus (engine/vendor/kati/testcase/*.mk) is also run by
engine/vendor/kati/tests/corpus.rs against the STANDALONE rkati binary — but
sarun is the deliverable, and a box runs make through a different pipeline:
the in-process `make` builtin (katirun.rs), brush recipes, the FUSE overlay,
seed_env export prefixes, in-process recursive $(MAKE). This runner drives
every corpus case through `sarun run -b -- make` and diffs its output against
real GNU make, with the same normalizers the rust runner uses.

Expected-fail bookkeeping:
  * a case whose first comment is `# TODO` / `# TODO(rust|all)` is xfail here
    too (same convention as corpus.rs);
  * BOX_XFAIL lists cases that legitimately diverge ONLY in a box (with the
    reason). Anything else must match byte-for-byte post-normalization —
    fail==0 is asserted, so a kati/brush/box regression cannot land silently.

Run:
    uv run --with "pyfuse3>=3.2" --with "trio>=0.22" --with "wcmatch>=8.4" \
      --with "python-magic>=0.4" python test_kati_corpus_box_rs.py
Env:
    KATI_CORPUS_ONLY=substr   run only matching cases
    KATI_CORPUS_DEBUG=1       print raw/normalized outputs for failures
"""
import concurrent.futures, os, re, shutil, socket, subprocess, sys, tempfile, time
from pathlib import Path
from importlib.machinery import SourceFileLoader

_HERE = Path(__file__).resolve().parent
SARUN = str(_HERE / "libtestsarun.py")
BIN = _HERE.parent / "engine/target/x86_64-unknown-linux-musl/release/sarun"
TESTCASES = _HERE.parent / "engine/vendor/kati/testcase"

# Box-only expected failures, with reasons. Keep this SHORT — every entry is
# a known, understood divergence between a box run and real make, not a bug
# parked out of sight.
BOX_XFAIL: dict[str, str] = {
}

_fails: list[str] = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


# ── normalizers (ported from engine/vendor/kati/tests/corpus.rs) ─────────────

def _norms(pairs):
    return [(re.compile(p, re.M), r) for (p, r) in pairs]

QUOTES = (r"[`'\"‘’]", '"')

MAKE_NORMS = _norms([
    QUOTES,
    (r"make(?:\[\d+\])?: (Entering|Leaving) directory[^\n]*\n", ""),
    (r"make(?:\[\d+\])?: ", ""),
    (r'"[^"\n]+" is up to date\.\n', ""),
    (r" recipe for target ", " commands for target "),
    (r" recipe commences ", " commands commence "),
    (r"missing rule before recipe\.", "missing rule before commands."),
    (r" \(did you mean TAB instead of 8 spaces\?\)", ""),
    (r"Extraneous text after", "extraneous text after"),
    (r"\s+Stop\.", ""),
    (r'Makefile:\d+: commands for target ".*?" failed\n', ""),
    (r"/bin/(ba)?sh: line 1: ", ""),
    (r"/bin/(ba)?sh: \d+: ", ""),
    (r'(: \S+: No such file or directory)\n\*\*\* No rule to make target "[^"]+"\.', r"\1"),
    (r"\[\S+:\d+: ", "["),
    (r"ninja: warning: [^\n]+", ""),
])

KATI_NORMS = _norms([
    QUOTES,
    # the box banner: `sarun-engine: box N  (overlay root: …)  UI connected`
    (r"sarun-engine: box \d+[^\n]*\n", ""),
    (r"make(?:\[\d+\])?: (Entering|Leaving) directory[^\n]*\n", ""),
    (r"make(?:\[\d+\])?: ", ""),
    (r"\*kati\*[^\n]*", ""),
    (r"c?kati: ", ""),
    (r"/bin/(ba)?sh: line 1: ", ""),
    (r"/bin/(ba)?sh: \d+: ", ""),
    (r"/bin/sh: ", ""),
    (r".*: warning for parse error in an unevaluated line: [^\n]*", ""),
    (r"([^\n ]+: )?FindEmulator: ", ""),
    (r" (\./+)+kati\.\S+", ""),
    (r" (\./+)+test\S+\.json", ""),
    (r"(: )open (\S+): n(o such file or directory)\nNOTE:[^\n]*", r"\1\2: N\3"),
    (r"Too many symbolic links encountered", "Too many levels of symbolic links"),
    (r" \(os error \d+\)", ""),
])

CIRC = re.compile(r"(Circular .* dropped\.\n)")

def normalize(text: str, norms) -> str:
    prefix = "".join(m.group(1) for m in CIRC.finditer(text))
    body = CIRC.sub("", text)
    out = prefix + body
    for (rx, rep) in norms:
        out = rx.sub(rep, out)
    return out


TODO_RE = re.compile(r"^# TODO(?:\(([-a-z|]+)(?:/([-a-z0-9|]+))?\))?")

def xfail_reason(src: str):
    """Same convention as corpus.rs::xfail_reason (tags rust|all apply)."""
    for line in src.splitlines():
        if not line.startswith("#!") and not line.startswith("# TODO"):
            return None
        m = TODO_RE.match(line)
        if not m:
            continue
        if m.group(2):
            continue  # sub-test-scoped; we run only the default goal
        tags = m.group(1) or ""
        if not tags:
            return line
        if "rust" in tags.split("|") or "all" in tags.split("|"):
            return line
    return None


def wipe_artifacts(d: Path):
    """Everything except the Makefile and staged symlinks."""
    for e in d.iterdir():
        if e.name == "Makefile" or e.is_symlink():
            continue
        if e.is_dir():
            shutil.rmtree(e, ignore_errors=True)
        else:
            e.unlink(missing_ok=True)


def run_case(name: str, src_path: Path, work_root: Path, idx: int = 0):
    """Returns (name, verdict, detail) — verdict in pass/fail/xfail/xpass."""
    src = src_path.read_text(errors="replace")
    xfail = xfail_reason(src) or BOX_XFAIL.get(name)

    work = work_root / name.removesuffix(".mk")
    shutil.rmtree(work, ignore_errors=True)
    work.mkdir(parents=True)
    shutil.copy(src_path, work / "Makefile")
    for sub in ("submake", "dump", "tools"):
        if sub in src and (TESTCASES / sub).is_dir():
            (work / sub).symlink_to(TESTCASES / sub)

    env = dict(os.environ)
    env["MAKEFLAGS"] = "SHELL=/bin/bash"
    try:
        mk = subprocess.run(["make"], cwd=work, env=env,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                            timeout=60)
        mk_out = mk.stdout.decode(errors="replace")
    except subprocess.TimeoutExpired:
        mk_out = "<make timeout>"
    wipe_artifacts(work)

    try:
        bx = subprocess.run(
            [str(BIN), "run", "-b", f"C{idx}_{name.removesuffix('.mk')[:20]}",
             "-C", str(work), "--", "make"],
            env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            timeout=120)
        bx_out = bx.stdout.decode(errors="replace")
    except subprocess.TimeoutExpired:
        bx_out = "<box timeout>"
    shutil.rmtree(work, ignore_errors=True)

    # Box builtins resolve relative paths against the logical cwd by
    # absolutizing, so their error messages can show the full path where GNU
    # shows the path as typed — strip the per-case workdir prefix from BOTH
    # sides (legitimate absolute-path output, e.g. $(abspath), stays equal).
    bx_out = bx_out.replace(str(work) + "/", "")
    mk_out = mk_out.replace(str(work) + "/", "")
    mk_n = normalize(mk_out, MAKE_NORMS)
    bx_n = normalize(bx_out, KATI_NORMS)
    same = mk_n == bx_n
    if xfail:
        return (name, "xpass" if same else "xfail", xfail)
    if same:
        return (name, "pass", "")
    detail = ""
    if os.environ.get("KATI_CORPUS_DEBUG"):
        detail = (f"\n--- make raw ---\n{mk_out}\n--- make norm ---\n{mk_n}"
                  f"\n--- box raw ---\n{bx_out}\n--- box norm ---\n{bx_n}")
    return (name, "fail", detail)


def wait_socket(sock, timeout=30):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.settimeout(1.0); s.connect(sock); return True
        except OSError:
            time.sleep(0.1)
    return False


def main():
    if not BIN.exists():
        raise SystemExit("engine binary missing — run `make engine`")
    tmp = Path(tempfile.mkdtemp(prefix="corpusbox-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "RS"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    # Box-visible work root (host /tmp is tmpfs-hidden box-side).
    work_root = Path("/root/corpusbox_work")
    shutil.rmtree(work_root, ignore_errors=True)
    work_root.mkdir(parents=True)

    only = os.environ.get("KATI_CORPUS_ONLY")
    cases = sorted(p for p in TESTCASES.glob("*.mk")
                   if not only or only in p.name)

    eng = subprocess.Popen([str(BIN), "serve"],
                           stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT)
    tally = {"pass": 0, "fail": 0, "xfail": 0, "xpass": 0}
    failures, xpasses = [], []
    try:
        if not wait_socket(m.sock_path()):
            raise RuntimeError("engine socket never appeared")
        with concurrent.futures.ThreadPoolExecutor(max_workers=6) as ex:
            futs = [ex.submit(run_case, p.name, p, work_root, i)
                    for i, p in enumerate(cases)]
            first_fail = []
            for f in concurrent.futures.as_completed(futs):
                name, verdict, detail = f.result()
                if verdict == "fail":
                    first_fail.append(name)
                    continue
                tally[verdict] += 1
                if verdict == "xpass":
                    xpasses.append(name)
        # Re-run first-sweep failures SERIALLY: box output interleaves under
        # engine load (recipe chunks vs. the client banner), which can split
        # lines mid-stream — a deterministic regression still fails here.
        os.environ["KATI_CORPUS_DEBUG"] = os.environ.get("KATI_CORPUS_DEBUG", "1")
        for i, name in enumerate(first_fail):
            p = TESTCASES / name
            name2, verdict, detail = run_case(name, p, work_root, 9000 + i)
            tally[verdict] += 1
            if verdict == "fail":
                failures.append(name)
                print(f" FAIL {name}{detail}")
            elif verdict == "xpass":
                xpasses.append(name)
    finally:
        eng.terminate()
        try: eng.wait(timeout=10)
        except Exception: eng.kill()
        shutil.rmtree(work_root, ignore_errors=True)
        shutil.rmtree(tmp, ignore_errors=True)

    total = sum(tally.values())
    print(f"BOX_CORPUS_PASS={tally['pass']}/{total}  "
          f"fail={tally['fail']} xfail={tally['xfail']} xpass={tally['xpass']}")
    if xpasses:
        print("    xpass (TODO headers stale for box mode — informational):")
        for n in sorted(xpasses):
            print(f"        {n}")
    check(tally["fail"] == 0,
          f"box corpus: every non-TODO case matches GNU make "
          f"(failures: {sorted(failures)})")
    print("\n" + ("KATI-CORPUS-BOX PASS" if not _fails
                  else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_kati_corpus_box_rs():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
