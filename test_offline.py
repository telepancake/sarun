#!/usr/bin/env python3
"""Offline tests (no FUSE mount, no UI event loop) for consolidate-from-index and
Supervisor change accounting. Run:
    /home/user/venv/bin/python test_offline.py
"""
import os, sys, tempfile, shutil, sqlite3, json, stat as stat_mod, time
from pathlib import Path
from importlib.machinery import SourceFileLoader

m = SourceFileLoader("slopbox", "/home/user/sarun/sarun").load_module()

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


def _redirect_state(tmp):
    """Point state_home/live_home at a temp dir so we don't touch the real ~/."""
    os.environ["XDG_STATE_HOME"] = str(tmp / "state")
    os.environ["XDG_RUNTIME_DIR"] = str(tmp / "run")
    os.environ["XDG_CONFIG_HOME"] = str(tmp / "config")
    os.environ["XDG_DATA_HOME"] = str(tmp / "data")


def test_consolidate_from_index():
    tmp = Path(tempfile.mkdtemp(prefix="ofl-"))
    _redirect_state(tmp)
    try:
        sid = "20260604-000000_111"
        backing = m.live_dir(sid); up = backing / "up"
        up.mkdir(parents=True)
        idx = m.Index(backing)
        wid = idx.writer_for(os.getpid())

        # a created TEXT file -> should fold into the patch
        (up / "newfile.txt").write_text("hello\nworld\n")
        idx.set_entry("newfile.txt", "file", stat_mod.S_IFREG | 0o644, wid, "create")
        # a created BINARY file -> sqlar
        (up / "blob.bin").write_bytes(bytes(range(256)))
        idx.set_entry("blob.bin", "file", stat_mod.S_IFREG | 0o644, wid, "create")
        # a symlink -> sqlar
        os.symlink("/some/target", up / "lnk")
        idx.set_entry("lnk", "symlink", stat_mod.S_IFLNK | 0o777, wid, "symlink")
        # a deletion of a host file -> sqlar tombstone, sourced from the INDEX
        assert Path("/etc/hostname").exists()
        idx.set_entry("etc/hostname", "whiteout", 0, wid, "unlink")

        m.consolidate(str(backing), sid, ["echo", "hi"], index=idx)

        # patch holds the text file
        pf = m.patch_path(sid)
        check(pf.exists(), "patch.xz written")
        files = m.parse_patch(m.read_patch_file(pf))
        check("newfile.txt" in files, "text file folded into the patch")

        # sqlar holds blob, symlink, tombstone
        sp = m.sqlar_path(sid)
        names = {n for n, _mode, _mt, _sz in m.sqlar_list(sp)}
        check("blob.bin" in names, "binary file in sqlar")
        check("lnk" in names, "symlink in sqlar")
        check("etc/hostname" in names, "index whiteout -> sqlar tombstone")
        tmode = m.sqlar_mode(sp, "etc/hostname")
        check(tmode is not None and stat_mod.S_ISCHR(tmode),
              "tombstone is a char-device entry")

        # provenance table carried into the sqlar
        conn = sqlite3.connect(str(sp))
        try:
            rows = conn.execute(
                "SELECT path,pid,argv FROM provenance ORDER BY path").fetchall()
        finally:
            conn.close()
        provpaths = {r[0] for r in rows}
        check("blob.bin" in provpaths, "provenance recorded for binary file")
        if rows:
            r0 = next(r for r in rows if r[0] == "blob.bin")
            check(r0[1] == os.getpid(), "provenance pid matches the writer")
            check(json.loads(r0[2]) and isinstance(json.loads(r0[2]), list),
                  "provenance argv is a non-empty list")
        idx.close()
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_consolidate_opaque_expands_tombstones():
    tmp = Path(tempfile.mkdtemp(prefix="ofl-"))
    _redirect_state(tmp)
    try:
        # pick a small non-empty lower dir
        lower = None
        for cand in ("/etc/profile.d", "/etc/skel"):
            p = Path(cand)
            if p.is_dir() and any(p.iterdir()): lower = cand; break
        if lower is None:
            check(True, "opaque: no candidate lower dir (skipped)"); return
        rel = lower.lstrip("/")
        sid = "20260604-000000_222"
        backing = m.live_dir(sid); up = backing / "up"
        (up / rel).mkdir(parents=True)
        idx = m.Index(backing)
        wid = idx.writer_for(os.getpid())
        # opaque dir + one re-materialized child
        idx.set_entry(rel, "dir", stat_mod.S_IFDIR | 0o755, wid, "mkdir", opaque=1)
        (up / rel / "kept.txt").write_text("x")
        idx.set_entry(rel + "/kept.txt", "file", stat_mod.S_IFREG | 0o644, wid, "create")

        m.consolidate(str(backing), sid, ["sh"], index=idx)
        sp = m.sqlar_path(sid)
        names = {n for n, _m, _mt, _sz in m.sqlar_list(sp)}
        # every original lower child (that wasn't re-materialized) gets a tombstone
        lower_children = [os.path.relpath(os.path.join(r, n), "/")
                          for r, ds, fs in os.walk(lower) for n in ds + fs]
        some_tombstoned = any(c in names and stat_mod.S_ISCHR(m.sqlar_mode(sp, c))
                              for c in lower_children)
        check(some_tombstoned, "opaque dir expanded into per-child tombstones")
        check(rel + "/kept.txt" not in names or not stat_mod.S_ISCHR(
                  m.sqlar_mode(sp, rel + "/kept.txt") or 0),
              "re-materialized child is NOT tombstoned")
        idx.close()
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_supervisor_no_mount_register_fails_closed():
    sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=None)
    # a VALID sid so we exercise the mount-fail-closed path (not the sid rejection)
    ack = sup.register(dict(session_id="20260604-000000_111", cmd=["true"]))
    check(ack.get("ok") is False, "register without a mount fails closed (no ok)")
    check("error" in ack, "register failure carries an error message")


if __name__ == "__main__":
    for t in (test_consolidate_from_index, test_consolidate_opaque_expands_tombstones,
              test_supervisor_no_mount_register_fails_closed):
        print(f"\n== {t.__name__} ==")
        try:
            t()
        except Exception as e:
            import traceback; traceback.print_exc(); _fails.append(f"{t.__name__}: {e}")
    print("\n" + ("ALL PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    sys.exit(1 if _fails else 0)
