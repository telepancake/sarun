#!/usr/bin/env python3
"""Rotation (DEPOT-DESIGN.md §6) against the RUST engine: promote a child
box over its parent. Encodings are rewritten; NO view changes — including
the parts of the view that come from the LIVE host backdrop, which is what
holes exist for (a snapshot could not pass the liveness check below).

Real-effect assertions:
  • parent view (now the child box): the old stack's total occlusion
  • child view (old parent): its OWN old occlusion — overwrites restored,
    the child's additions gone
  • LIVENESS: a host file the child overwrote reads back through the
    rotated old-parent LIVE — changing it on the host changes the view

Needs FUSE + bwrap.
"""
import json, os, shutil, socket, sqlite3, subprocess, tempfile, time
from pathlib import Path
from sarun_test_paths import ENGINE_BIN
from importlib.machinery import SourceFileLoader

_HERE = Path(__file__).resolve().parent
SARUN = str(_HERE / "libtestsarun.py")
BIN = ENGINE_BIN

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)

def wait_socket(sock, timeout=30):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.settimeout(1.0); s.connect(sock); return True
        except OSError:
            time.sleep(0.1)
    return False

def run_in(name, cmd):
    return subprocess.run([str(BIN), "run", name, "--", "sh", "-c", cmd],
                          capture_output=True, text=True, timeout=60)

def main():
    if not BIN.exists():
        raise SystemExit("test_rotate_rs: engine binary missing — make engine")
    tmp = Path(tempfile.mkdtemp(prefix="rot-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "ROTV"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    # Host-side zone OUTSIDE /tmp: boxes mount their own tmpfs over /tmp,
    # so a /tmp path would never reach the real host backdrop.
    hostzone = Path(tempfile.mkdtemp(prefix="rot-host-", dir=os.path.expanduser("~")))
    hostf = hostzone / "live.txt"; hostf.write_text("H1\n")
    eng = None
    try:
        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        sock = m.sock_path()
        if not wait_socket(sock):
            raise RuntimeError("engine socket never appeared")

        # Parent A and child B as separate captures; parenthood is stamped
        # at rest (the same way test_nested_apply builds stacks — the CLI
        # `run` has no child-creation form).
        r = run_in("A", "mkdir -p /rot && echo A > /rot/a-only.txt && "
                        "echo OLD > /rot/over.txt")
        check(r.returncode == 0, f"parent A run (rc={r.returncode}: {r.stderr[:150]})")
        r = run_in("B", f"mkdir -p /rot && echo NEW > /rot/over.txt && "
                        f"echo B > /rot/b-new.txt && echo BH > {hostf}")
        check(r.returncode == 0, f"child B run (rc={r.returncode}: {r.stderr[:150]})")
        aid = m.sync_request(sock, type="ui", verb="resolve_box", args=["A"])["r"]
        bid = m.sync_request(sock, type="ui", verb="resolve_box", args=["B"])["r"]
        m.sqlar_meta_set(m.sqlar_path(bid), "parent_box_id", str(aid))
        # Rotate the at-rest pair (the verb hydrates both boxes itself).
        rep = m.sync_request(sock, type="ui", verb="rotate", args=[bid])
        check(rep and rep.get("r", {}).get("ok") is True,
              f"rotate verb succeeds (got {rep!r})")

        # New parent (the old child box B): the old stack's total view.
        # (Display names are dotted paths: B is now top-level, A is B.A.)
        r = run_in("B", "cat /rot/over.txt /rot/b-new.txt /rot/a-only.txt "
                        f"{hostf}")
        check(r.returncode == 0, f"parent view readable (rc={r.returncode}: {r.stderr[:150]})")
        check(r.stdout == "NEW\nB\nA\nBH\n",
              f"parent view = old stack's occlusion (got {r.stdout!r})")

        # Old parent, now the child: ITS old view — B's changes gone.
        r = run_in("B.A", "cat /rot/over.txt /rot/a-only.txt")
        check(r.returncode == 0, f"child view readable (rc={r.returncode}: {r.stderr[:150]})")
        check(r.stdout == "OLD\nA\n",
              f"child view restores old parent occlusion (got {r.stdout!r})")
        r = run_in("B.A", "test -e /rot/b-new.txt")
        check(r.returncode != 0, "child's addition invisible from the old parent")

        # THE liveness check: the hole resolves the host file LIVE.
        r = run_in("B.A", f"cat {hostf}")
        check(r.stdout == "H1\n", f"hole shows host content (got {r.stdout!r})")
        # Change the file on the host, then restart the engine (dropping
        # the persistent FUSE mount and its kernel page cache — the
        # cache, not any store, is the only thing holding old bytes).
        hostf.write_text("H2\n")
        eng.terminate()
        try: eng.wait(timeout=5)
        except subprocess.TimeoutExpired: eng.kill()
        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        check(wait_socket(sock), "engine restarted")
        r = run_in("B.A", f"cat {hostf}")
        check(r.stdout == "H2\n",
              f"hole is LIVE — host change visible after restart, nothing "
              f"snapshotted at rest (got {r.stdout!r})")

        # Parenthood flipped (bookkeeping).
        with sqlite3.connect(m.sqlar_path(aid)) as c:
            row = c.execute("SELECT value FROM meta WHERE key='parent_box_id'").fetchone()
        check(row is not None and row[0] == str(bid),
              f"old parent now hangs off the promoted child (got {row!r})")
    finally:
        if eng:
            eng.terminate()
            try: eng.wait(timeout=5)
            except subprocess.TimeoutExpired: eng.kill()
        shutil.rmtree(tmp, ignore_errors=True)
        shutil.rmtree(hostzone, ignore_errors=True)
    if _fails:
        raise AssertionError(_fails)
    print("test_rotate_rs: all checks passed")

def test_rotate_rs():
    main()

if __name__ == "__main__":
    main()
