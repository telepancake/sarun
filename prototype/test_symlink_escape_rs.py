#!/usr/bin/env python3
"""Adversarial RUST-engine test: apply must NOT follow a box-planted ancestor
symlink onto the host (audit C2 / H6). The old Python `test_symlink_escape.py`
was deleted with the prototype program; the Rust engine had NO symlink-escape
test, yet apply-through-ancestor-symlink is a real host-escape class.

The attack: a box captures BOTH a symlink `<link> -> /` and a regular file
`<link>/<evil>`. If `review.apply` resolved the file's parent by FOLLOWING the
box-planted `<link>` symlink, the write would land at the host root `/<evil>` —
a full escape from the box's capture into an arbitrary host location. The C2 fix
resolves every parent component with `O_NOFOLLOW` (`hostfs::parent_beneath`), so
the apply of `<link>/<evil>` must REFUSE rather than traverse the symlink.

This test drives the real engine binary: it builds a box (via the Python sqlar
helpers in libtestsarun.py) that plants `<link> -> /` and `<link>/<evil>`,
applies the symlink, then applies the file, and asserts the host `/<evil>` was
NEVER created and that the file apply was reported as an error (refused), not
silently succeeded.

Run:
    uv run --with "pyfuse3>=3.2" --with "trio>=0.22" --with "wcmatch>=8.4" \
      --with "python-magic>=0.4" python test_symlink_escape_rs.py

FAILS LOUD (raises) if the engine binary cannot be built — no fake-green skip.
"""
import os, socket, stat as stat_mod, subprocess, sys
import tempfile, shutil, time
from pathlib import Path
from importlib.machinery import SourceFileLoader

SARUN = "/home/user/sarun/prototype/libtestsarun.py"
CRATE = Path(__file__).resolve().parent.parent / "engine"
BIN = CRATE / "target/x86_64-unknown-linux-musl/release/sarun"

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


def call(m, sock, verb, *args):
    return m.sync_request(sock, type="ui", verb=verb, args=list(args))["r"]


def build_escape_box(m, sid, link_rel, evil_rel, content):
    """Build a FINISHED on-disk box that plants `link_rel -> /` (a symlink whose
    target is the host root) and a regular file `evil_rel` under it."""
    bk = m.live_dir(sid); (bk / "up").mkdir(parents=True)
    ix = m.Index(bk); w = ix.writer_for(os.getpid())
    # The hostile ancestor symlink: target = "/" (the host root).
    ix.set_entry(link_rel, "symlink", stat_mod.S_IFLNK | 0o777, w, "create",
                 target=b"/")
    # A regular file whose path traverses the planted symlink. If apply follows
    # the symlink, this lands at the host root as /<basename(evil_rel)>.
    ix.set_entry(evil_rel, "file", stat_mod.S_IFREG | 0o644, w, "create")
    bp = m.blob_path(ix.box_id, ix.row_id(evil_rel))
    bp.parent.mkdir(parents=True, exist_ok=True); bp.write_bytes(content)
    m.consolidate(str(bk), sid, index=ix); ix.close()
    shutil.rmtree(bk, ignore_errors=True)


def main():
    if not ensure_binary():
        raise SystemExit(
            "test_symlink_escape_rs: engine binary unavailable — run `make engine`")
    tmp = Path(tempfile.mkdtemp(prefix="symesc-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "SE"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()

    # Unique host-root names so a stray write is unambiguous and cleanup is safe.
    tag = f"sarun_symesc_{os.getpid()}_{int(time.time())}"
    link_name = f"{tag}_link"          # the box-planted symlink -> /
    evil_name = f"{tag}_evil"          # the escape target basename
    link_rel = link_name               # box rel path of the symlink
    evil_rel = f"{link_name}/{evil_name}"  # box rel path of the file under it
    host_link = Path("/") / link_name      # where the symlink materializes
    host_escape = Path("/") / evil_name    # where a SUCCESSFUL escape would land

    # Pre-clean any leftovers from a previous aborted run.
    for p in (host_escape, host_link):
        try:
            if p.is_symlink() or p.exists(): p.unlink()
        except OSError:
            pass

    eng = None
    try:
        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        sock = m.sock_path()
        if not wait_socket(sock):
            out = eng.stdout.read(2000) if eng.stdout else b""
            raise RuntimeError("rust engine socket never appeared:\n"
                               + out.decode(errors="replace"))

        sid = "9101"
        build_escape_box(m, sid, link_rel, evil_rel, b"PWNED\n")

        # 1) Apply the symlink itself. This legitimately materializes the box's
        #    captured `<link> -> /` at the host path /<link>. (The box owns this
        #    path; planting its own symlink is allowed — the escape would be in
        #    the NEXT step traversing it.)
        rsym = call(m, sock, "review.apply", sid, [link_rel])
        check(host_link.is_symlink(),
              "symesc-rs: the box's own symlink materialized at the host path")
        check(not rsym.get("errors"),
              f"symesc-rs: applying the symlink itself succeeds: {rsym}")

        # 2) Apply the file UNDER the symlink. The C2 guard must refuse to
        #    resolve the parent THROUGH the box-planted symlink, so the host
        #    root /<evil> must NOT be created and the apply must report an error.
        rfile = call(m, sock, "review.apply", sid, [evil_rel])
        check(not host_escape.exists(),
              "symesc-rs: apply did NOT follow the ancestor symlink onto /<evil> "
              "(host escape refused)")
        check(bool(rfile.get("errors")) and not rfile.get("applied"),
              f"symesc-rs: file-through-symlink apply is reported as REFUSED, "
              f"not applied: {rfile}")

        # Belt-and-suspenders: a whole-box apply (paths=null) must likewise leave
        # no /<evil> on the host.
        call(m, sock, "review.apply", sid, None)
        check(not host_escape.exists(),
              "symesc-rs: whole-box apply still leaves no escaped /<evil> on host")

        call(m, sock, "delete", sid)

        eng.terminate()
        try: eng.wait(timeout=10)
        except subprocess.TimeoutExpired:
            eng.kill(); eng.wait(timeout=5)
        check(eng.returncode == 0, "symesc-rs: SIGTERM exits 0")
    except Exception as e:
        import traceback; traceback.print_exc(); _fails.append(str(e))
    finally:
        if eng is not None and eng.poll() is None:
            eng.kill()
            try: eng.wait(timeout=5)
            except Exception: pass
        # Clean up anything we (or a buggy apply) left at the host root.
        for p in (host_escape, host_link):
            try:
                if p.is_symlink() or p.exists(): p.unlink()
            except OSError:
                pass
        os.environ.pop("SLOPBOX_NS", None)
        shutil.rmtree(tmp, ignore_errors=True)
    print("\n" + ("SYMLINK-ESCAPE-RS PASS" if not _fails
                  else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_symlink_escape_rs():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
