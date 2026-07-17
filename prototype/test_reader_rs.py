#!/usr/bin/env python3
"""Reader-pane byte feed through the REAL engine socket: the
`review.file_bytes` UI verb — what 'V' on the Changes pane feeds the
document reader — returns a box path's CURRENT bytes: the captured
write when the path is in the change set, the host file underneath
when it is not, and a LOUD {ok:false, error} for a path that exists
nowhere. Real-effect assertions: the base64 payload round-trips
byte-exact against what the box actually wrote.

Needs FUSE + bwrap (spawns a real box). Run:
    uv run --with "wcmatch>=8.4" --with "python-magic>=0.4" \\
      python test_reader_rs.py
"""
import base64, os, shutil, socket, subprocess, tempfile, time
from pathlib import Path
from sarun_test_paths import ENGINE_BIN
from importlib.machinery import SourceFileLoader

_HERE = Path(__file__).resolve().parent
SARUN = str(_HERE / "libtestsarun.py")
CRATE = _HERE.parent / "engine"
BIN = ENGINE_BIN

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


def ensure_binary() -> bool:
    if not BIN.exists():
        r = subprocess.run(["make", "engine"], cwd=CRATE.parent,
                           capture_output=True, text=True)
        if r.returncode != 0 or not BIN.exists():
            return False
    return True


def wait_socket(sock, timeout=30):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.settimeout(1.0); s.connect(sock); return True
        except OSError:
            time.sleep(0.1)
    return False


def newest_sqlar():
    return max(Path(os.environ["XDG_STATE_HOME"]).joinpath("slopbox.RDR")
               .glob("*.sqlar"), key=lambda p: int(p.stem))


def main():
    if not ensure_binary():
        raise SystemExit("test_reader_rs: engine binary unavailable")
    tmp = Path(tempfile.mkdtemp(prefix="rdr-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "RDR"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    eng = None
    try:
        # A host-side document the box never touches (the fallback branch).
        host_doc = tmp / "host-doc.md"
        host_doc.write_text("# Host Doc\n\nhello from the host\n")

        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if not wait_socket(m.sock_path()):
            out = eng.stdout.read(2000) if eng.stdout else b""
            raise RuntimeError("rust engine socket never appeared:\n"
                               + out.decode(errors="replace"))
        sock = m.sock_path()

        # The box writes a markdown doc — the captured-write branch. At /
        # (not under the test tmp dir): the box's /tmp is its own tmpfs,
        # while a root write is a plain captured overlay row.
        content = "# Box Doc\n\nwritten inside the box, [link](other.md)\n"
        r = subprocess.run(
            [str(BIN), "run", "READ", "--", "sh", "-c",
             r"printf '# Box Doc\n\nwritten inside the box, [link](other.md)\n'"
             " > /box-doc.md"],
            capture_output=True, text=True, timeout=60)
        check(r.returncode == 0,
              f"box run exits 0 (rc={r.returncode}: {r.stderr[:200]})")
        sid = int(newest_sqlar().stem)

        def file_bytes(rel):
            rep = m.sync_request(sock, type="ui", verb="review.file_bytes",
                                 args=[str(sid), rel]) or {}
            return rep.get("r") or {}

        # 1. captured write: byte-exact round-trip of what the box wrote.
        v = file_bytes("box-doc.md")
        check(v.get("ok") is True, f"captured write served (got {v!r})")
        got = base64.b64decode(v.get("b64", "")) if v.get("ok") else b""
        check(got == content.encode(),
              f"captured bytes round-trip exactly (got {got[:60]!r})")

        # 2. host fallback: a path outside the change set serves host bytes.
        v = file_bytes(str(host_doc).lstrip("/"))
        check(v.get("ok") is True, f"host file served (got {v!r})")
        got = base64.b64decode(v.get("b64", "")) if v.get("ok") else b""
        check(got == host_doc.read_bytes(),
              "host bytes round-trip exactly")

        # 3. a path that exists nowhere refuses loudly, never empty-success.
        v = file_bytes(str(tmp / "no-such-doc.md").lstrip("/"))
        check(v.get("ok") is False and v.get("error"),
              f"missing path is a loud error (got {v!r})")
    finally:
        if eng:
            eng.terminate()
            try: eng.wait(timeout=5)
            except subprocess.TimeoutExpired: eng.kill()
        shutil.rmtree(tmp, ignore_errors=True)

    if _fails:
        raise AssertionError(_fails)
    print("test_reader_rs: all checks passed")


def test_reader_rs():
    main()


if __name__ == "__main__":
    main()
