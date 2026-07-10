#!/usr/bin/env python3
"""Editor-pane save path through the REAL engine socket: the
`review.write_file` UI verb — what Ctrl-S on an 'E'-opened Changes file
sends — overwrites a box path's CURRENT bytes as a CAPTURED row (the
same copy_up → pool blob → finalize path the box's own writes take),
and the box's overlay serves the new bytes back: `review.file_bytes`
round-trips them byte-exact, and a CHILD box parented on the session
reads them through a real FUSE mount (`cat` sees the edit). The host
file underneath is never touched. Refusals are loud: symlinks, binary
payloads, and paths that exist nowhere all return {ok:false, error}.

Needs FUSE + bwrap (spawns real boxes). Run:
    uv run --with "wcmatch>=8.4" --with "python-magic>=0.4" \\
      python test_editor_rs.py
"""
import base64, os, shutil, socket, subprocess, tempfile, time
from pathlib import Path
from importlib.machinery import SourceFileLoader

_HERE = Path(__file__).resolve().parent
SARUN = str(_HERE / "libtestsarun.py")
CRATE = _HERE.parent / "engine"
BIN = CRATE / "target/x86_64-unknown-linux-musl/release/sarun"

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
    return max(Path(os.environ["XDG_STATE_HOME"]).joinpath("slopbox.EDT")
               .glob("*.sqlar"), key=lambda p: int(p.stem))


def main():
    if not ensure_binary():
        raise SystemExit("test_editor_rs: engine binary unavailable")
    tmp = Path(tempfile.mkdtemp(prefix="edt-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "EDT"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    eng = None
    try:
        # A host-side file the box never touches (the shadow-a-host-file
        # branch: the edit must land as a captured row, host unchanged).
        host_doc = tmp / "host-conf.yaml"
        host_doc.write_text("key: original\n")

        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if not wait_socket(m.sock_path()):
            out = eng.stdout.read(2000) if eng.stdout else b""
            raise RuntimeError("rust engine socket never appeared:\n"
                               + out.decode(errors="replace"))
        sock = m.sock_path()

        # The box writes a python file (the captured-write branch) and
        # plants a symlink (the refusal branch). At / — not under the test
        # tmp dir: the box's /tmp is its own tmpfs, while root writes are
        # plain captured overlay rows.
        original = "def answer():\n    return 41\n"
        r = subprocess.run(
            [str(BIN), "run", "EDIT", "--", "sh", "-c",
             r"printf 'def answer():\n    return 41\n' > /ed-code.py"
             " && ln -s /etc/hosts /ed-link"],
            capture_output=True, text=True, timeout=60)
        check(r.returncode == 0,
              f"box run exits 0 (rc={r.returncode}: {r.stderr[:200]})")
        sid = int(newest_sqlar().stem)

        def verb(name, *args):
            rep = m.sync_request(sock, type="ui", verb=name,
                                 args=[str(a) for a in args]) or {}
            return rep.get("r") or {}

        def b64(data: bytes) -> str:
            return base64.b64encode(data).decode()

        # 0. sanity: the captured row serves the box's own bytes.
        v = verb("review.file_bytes", sid, "ed-code.py")
        check(v.get("ok") is True
              and base64.b64decode(v.get("b64", "")) == original.encode(),
              f"pre-edit bytes are the box's write (got {v!r})")

        # 1. the editor save: overwrite the captured row's bytes.
        edited = "def answer():\n    return 42  # fixed in the editor\n"
        v = verb("review.write_file", sid, "ed-code.py", b64(edited.encode()))
        check(v.get("ok") is True, f"write_file succeeds (got {v!r})")

        # 2. the captured row's bytes CHANGED — file_bytes round-trips the
        #    edit byte-exact (the row, not some side copy).
        v = verb("review.file_bytes", sid, "ed-code.py")
        got = base64.b64decode(v.get("b64", "")) if v.get("ok") else b""
        check(got == edited.encode(),
              f"captured row serves the edited bytes (got {got[:60]!r})")

        # 3. the box serves the edit back THROUGH THE MOUNT: a child box
        #    parented on the session (the numeric-id-prefix stacking) reads
        #    the file via its real FUSE-mounted merged view.
        r = subprocess.run(
            [str(BIN), "run", f"{sid}.CHK", "--", "cat", "/ed-code.py"],
            capture_output=True, text=True, timeout=60)
        check(r.returncode == 0,
              f"child box run exits 0 (rc={r.returncode}: {r.stderr[:200]})")
        check(edited in r.stdout,
              f"child box cat sees the edit through the mount "
              f"(got {r.stdout[:80]!r})")

        # 4. editing a HOST file outside the change set: the edit lands as
        #    a NEW captured row; the host file is untouched.
        host_rel = str(host_doc).lstrip("/")
        v = verb("review.write_file", sid, host_rel, b64(b"key: edited\n"))
        check(v.get("ok") is True, f"host-shadow write succeeds (got {v!r})")
        v = verb("review.file_bytes", sid, host_rel)
        got = base64.b64decode(v.get("b64", "")) if v.get("ok") else b""
        check(got == b"key: edited\n",
              f"box view serves the shadowed edit (got {got!r})")
        check(host_doc.read_bytes() == b"key: original\n",
              "the HOST file is untouched by the box-layer save")

        # 5. loud refusals: symlink, binary payload, nowhere-path.
        v = verb("review.write_file", sid, "ed-link", b64(b"x"))
        check(v.get("ok") is False and "symlink" in str(v.get("error", "")),
              f"symlink refused loudly (got {v!r})")
        v = verb("review.write_file", sid, "ed-code.py", b64(b"a\0b"))
        check(v.get("ok") is False and "binary" in str(v.get("error", "")),
              f"NUL payload refused loudly (got {v!r})")
        nowhere = str(tmp / "no-such.py").lstrip("/")
        v = verb("review.write_file", sid, nowhere, b64(b"x"))
        check(v.get("ok") is False and v.get("error"),
              f"nowhere-path refused loudly (got {v!r})")
        # ...and none of the refusals corrupted the good row.
        v = verb("review.file_bytes", sid, "ed-code.py")
        got = base64.b64decode(v.get("b64", "")) if v.get("ok") else b""
        check(got == edited.encode(), "refusals left the row intact")
    finally:
        if eng:
            eng.terminate()
            try: eng.wait(timeout=5)
            except subprocess.TimeoutExpired: eng.kill()
        shutil.rmtree(tmp, ignore_errors=True)

    if _fails:
        raise AssertionError(_fails)
    print("test_editor_rs: all checks passed")


def test_editor_rs():
    main()


if __name__ == "__main__":
    main()
