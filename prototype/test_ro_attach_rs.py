#!/usr/bin/env python3
"""RO attachments (DEPOT-DESIGN.md §8) against the RUST engine: a box can
reference another box's layer READ-ONLY, conceptually between itself and its
parent. The attached layer's keys are served in the merged view; any mutation
of a matched key is EROFS, rejected before any capture side effect — which is
what guarantees the captured layer is independent of the attachment.

Real-effect assertions (never shape-only):
  • read-through: `cat` of an attached file lands its bytes in a captured row
  • EROFS: writing an attached key fails AND leaves no captured row
  • dir merge: a NEW key beside attached content captures normally
  • independence: the captured rows never include attachment keys

Needs FUSE + bwrap. Run:
    uv run --with "wcmatch>=8.4" --with "python-magic>=0.4" \\
      python test_ro_attach_rs.py
"""
import os, shutil, socket, sqlite3, stat as stat_mod, subprocess, sys, tempfile, time
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


def newest_sqlar():
    return max(Path(os.environ["XDG_STATE_HOME"]).joinpath("slopbox.ROT")
               .glob("*.sqlar"), key=lambda p: int(p.stem))


def rows(sp):
    with sqlite3.connect(f"file:{sp}?mode=ro", uri=True) as c:
        return {name: (rowid, mode, data) for rowid, name, mode, data in
                c.execute("SELECT rowid,name,mode,data FROM sqlar")}


def row_bytes(m, sp, name):
    r = rows(sp).get(name)
    if r is None:
        return None
    rowid, _mode, data = r
    if data is not None:
        return bytes(data)
    bp = m.blob_path(int(sp.stem), rowid)
    return bp.read_bytes() if bp.exists() else b""


def make_ro_box(m, sid, entries):
    """Engine-format at-rest box: sqlar rows with data NULL for files, the
    bytes in pool blobs (DESIGN.md D4) — the same rest form the Rust engine
    writes and reads."""
    sp = m.sqlar_path(sid)
    sp.parent.mkdir(parents=True, exist_ok=True)
    dirs = set()
    for rel, _ in entries:
        p = Path(rel).parent
        while str(p) not in (".", ""):
            dirs.add(str(p)); p = p.parent
    with sqlite3.connect(sp) as c:
        c.execute("CREATE TABLE IF NOT EXISTS sqlar(name TEXT PRIMARY KEY,"
                  " mode INT, mtime INT, sz INT, data BLOB,"
                  " opaque INT DEFAULT 0, writer INT, last_writer INT)")
        c.execute("CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,"
                  " value TEXT)")
        for d in sorted(dirs):
            c.execute("INSERT INTO sqlar(name,mode,mtime,sz,data) "
                      "VALUES(?,?,0,0,NULL)",
                      (d, stat_mod.S_IFDIR | 0o755))
        for rel, content in entries:
            cur = c.execute("INSERT INTO sqlar(name,mode,mtime,sz,data) "
                            "VALUES(?,?,0,?,NULL)",
                            (rel, stat_mod.S_IFREG | 0o644, len(content)))
            bp = m.blob_path(int(sid), cur.lastrowid)
            bp.parent.mkdir(parents=True, exist_ok=True)
            bp.write_bytes(content)
    return sp


def main():
    if not ensure_binary():
        raise SystemExit("test_ro_attach_rs: engine binary unavailable — run `make engine`")
    tmp = Path(tempfile.mkdtemp(prefix="rort-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "ROT"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    eng = None
    try:
        # The RO layer: an at-rest box with an "sdk" subtree.
        ro_sid = "8901"
        make_ro_box(m, ro_sid, [("sdk/tool.txt", b"SDKv1"),
                                ("sdk/lib/data.bin", b"\x00\x01\x02")])

        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if not wait_socket(m.sock_path()):
            out = eng.stdout.read(2000) if eng.stdout else b""
            raise RuntimeError("rust engine socket never appeared:\n"
                               + out.decode(errors="replace"))

        # Create the working box with a first trivial run, then stamp the
        # attachment into its meta (the load_mirror path restores it on
        # rerun — same persistence a live ro_attach verb writes).
        r = subprocess.run([str(BIN), "run", "WORK", "--", "true"],
                           capture_output=True, text=True, timeout=60)
        check(r.returncode == 0, f"setup run exits 0 (got {r.returncode}: {r.stderr[:200]})")
        sp = newest_sqlar()
        m.sqlar_meta_set(sp, "ro_attachments", f"[{ro_sid}]")

        # ── read-through: attached bytes visible in the merged view ────────
        r = subprocess.run(
            [str(BIN), "run", "WORK", "--",
             "sh", "-c", "cat /sdk/tool.txt > /captured-copy.txt"],
            capture_output=True, text=True, timeout=60)
        check(r.returncode == 0,
              f"read of attached file succeeds (rc={r.returncode}: {r.stderr[:200]})")
        check(row_bytes(m, sp, "captured-copy.txt") == b"SDKv1",
              f"attached bytes read through into a captured row "
              f"(got {row_bytes(m, sp, 'captured-copy.txt')!r})")

        # ── EROFS on matched keys; no capture side effect ───────────────────
        r = subprocess.run(
            [str(BIN), "run", "WORK", "--",
             "sh", "-c", "echo overwrite > /sdk/tool.txt"],
            capture_output=True, text=True, timeout=60)
        check(r.returncode != 0, "write to attached key fails")
        check("sdk/tool.txt" not in rows(sp),
              "rejected write left NO captured row (independence)")
        r = subprocess.run(
            [str(BIN), "run", "WORK", "--", "rm", "/sdk/lib/data.bin"],
            capture_output=True, text=True, timeout=60)
        check(r.returncode != 0, "unlink of attached key fails")
        check("sdk/lib/data.bin" not in rows(sp),
              "rejected unlink left NO whiteout row")

        # ── dir merge: new keys beside attached content capture normally ───
        r = subprocess.run(
            [str(BIN), "run", "WORK", "--",
             "sh", "-c", "echo out > /sdk/build-output.txt"],
            capture_output=True, text=True, timeout=60)
        check(r.returncode == 0,
              f"new key under attached dir succeeds (rc={r.returncode}: {r.stderr[:200]})")
        check(row_bytes(m, sp, "sdk/build-output.txt") == b"out\n",
              "new key captured in the box's own layer")

        # ── independence: no attachment key ever entered the captured layer ─
        captured = set(rows(sp))
        check(not any(k in captured for k in ("sdk/tool.txt", "sdk/lib/data.bin")),
              "captured layer contains no attachment keys")
        # The RO box itself is untouched.
        ro_rows = rows(m.sqlar_path(ro_sid))
        check(row_bytes(m, m.sqlar_path(ro_sid), "sdk/tool.txt") == b"SDKv1",
              "attachment layer unmodified")
        check(len(ro_rows) >= 2, "attachment layer rows intact")
    finally:
        if eng:
            eng.terminate()
            try: eng.wait(timeout=5)
            except subprocess.TimeoutExpired: eng.kill()
        shutil.rmtree(tmp, ignore_errors=True)

    if _fails:
        raise AssertionError(_fails)
    print("test_ro_attach_rs: all checks passed")


def test_ro_attach_rs():
    main()


if __name__ == "__main__":
    main()
