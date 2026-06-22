#!/usr/bin/env python3
"""END-TO-END behavioural proof that a PROCESS-SCOPED passthrough file rule
(full clause grammar — `passthrough <glob> and arg:<glob>`) fires host-direct
ONLY for the writer whose provenance matches, exercising the real overlay
host-direct WRITE routing (overlay.rs is_passthrough -> Rules::decide with the
writer's /proc facets).

Two boxes write two passthrough-glob-matching .key files:
  - writer A's argv MATCHES the `arg:` clause  -> host-direct: lands on the REAL
    HOST, NOT captured in the box;
  - writer B's argv does NOT match            -> CAPTURED in the box, host
    untouched.
That is the proc-scoped passthrough working end to end — a behaviour the old
path-only engine could not express.

Run:
    uv run --with "pyfuse3>=3.2" --with "trio>=0.22" --with "wcmatch>=8.4" \
      --with "python-magic>=0.4" python test_proc_passthrough_rs.py
Skips (passes vacuously) if cargo/binary/FUSE unavailable.
"""
import os, shutil, socket, subprocess, sys, tempfile, time
from pathlib import Path
from importlib.machinery import SourceFileLoader

_HERE = Path(__file__).resolve().parent
SARUN = str(_HERE / "libtestsarun.py")
CRATE = _HERE.parent / "engine"
BIN = CRATE / "target/x86_64-unknown-linux-musl/release/sarun"

A = Path("/root/pp_match.key")     # written by a MATCHING writer  -> host-direct
B = Path("/root/pp_other.key")     # written by a NON-matching writer -> captured

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


def ensure_binary() -> bool:
    if BIN.exists():
        return True
    if shutil.which("cargo") is None:
        return False
    r = subprocess.run(["make", "engine"], cwd=CRATE.parent,
                       capture_output=True, text=True)
    return r.returncode == 0 and BIN.exists()


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
    if not ensure_binary():
        raise SystemExit("test_proc_passthrough_rs: engine binary unavailable — run `make engine`")
    tmp = Path(tempfile.mkdtemp(prefix="proc-pt-"))
    for k, sub in (("XDG_STATE_HOME", "st"), ("XDG_RUNTIME_DIR", "rn"),
                   ("XDG_CONFIG_HOME", "cf"), ("XDG_DATA_HOME", "d")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "PROCPT"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    rules = Path(os.environ["XDG_CONFIG_HOME"]) / "slopbox.PROCPT" / "filerules"
    rules.parent.mkdir(parents=True, exist_ok=True)
    # proc-scoped passthrough: only a writer whose argv contains a token glob-
    # matching *PPMARK* is host-direct. The writer is the shell running the
    # redirect, so its argv carries the -c script text — we mark one of them.
    # `arg:PPMARK` matches a writer with a standalone PPMARK argv element. The
    # writer is the shell running the redirect; we pass PPMARK as $0 so it is a
    # whole argv token (a glob `*` does not cross the `/` in the -c script text).
    rules.write_text("passthrough root/pp_*.key and arg:PPMARK\n")
    for p in (A, B): p.unlink(missing_ok=True)
    eng = None

    def box(cmd, mark=False):
        argv = [str(BIN), "run", "--", "sh", "-c", cmd]
        if mark: argv.append("PPMARK")          # sets $0 -> a standalone argv token
        return subprocess.run(argv, capture_output=True, text=True, timeout=120)

    def latest_with(relname):
        got = None
        for sp in sorted(Path(os.environ["XDG_STATE_HOME"]).joinpath("slopbox.PROCPT")
                         .glob("*.sqlar"), key=lambda p: int(p.stem)):
            c = m.sqlar_content(sp, relname)
            if c is not None:
                got = c
        return got
    try:
        eng = subprocess.Popen([str(BIN), "serve"], env=dict(os.environ),
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if not wait_socket(m.sock_path()):
            raise RuntimeError("engine socket never appeared")

        # A) MATCHING writer: argv carries the PPMARK token -> host-direct.
        box("printf MATCH > /root/pp_match.key", mark=True)
        time.sleep(0.2)
        check(A.exists() and A.read_bytes() == b"MATCH",
              "proc-pt: matching-writer passthrough landed on the REAL HOST")
        check(latest_with("root/pp_match.key") is None,
              "proc-pt: matching-writer passthrough was NOT captured (host-direct)")

        # B) NON-matching writer: no PPMARK token -> captured, host untouched.
        box("printf NOPE > /root/pp_other.key")
        time.sleep(0.2)
        check(not B.exists(),
              "proc-pt: non-matching writer did NOT touch the host (captured)")
        check(latest_with("root/pp_other.key") == b"NOPE",
              "proc-pt: non-matching writer's file WAS captured in the box")
    finally:
        for p in (A, B): p.unlink(missing_ok=True)
        if eng is not None and eng.poll() is None:
            eng.terminate()
            try: eng.wait(timeout=10)
            except Exception: eng.kill()
        os.environ.pop("SLOPBOX_NS", None)
        shutil.rmtree(tmp, ignore_errors=True)
    print("\n" + ("PROC-PT PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_proc_passthrough_rs():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
