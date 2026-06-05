#!/usr/bin/env python3
"""Offline tests (no FUSE mount, no UI event loop) for the SINGLE per-instance db:
NULL-blob-then-consolidate lifecycle, process/env dedup, file->process tagging, and
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
    os.environ["XDG_STATE_HOME"] = str(tmp / "state")
    os.environ["XDG_RUNTIME_DIR"] = str(tmp / "run")
    os.environ["XDG_CONFIG_HOME"] = str(tmp / "config")
    os.environ["XDG_DATA_HOME"] = str(tmp / "data")


def _names(sp):
    return {n for n, _mode, _mt, _sz in m.sqlar_list(sp)}


def test_one_db_only_and_blob_lifecycle():
    """Exactly ONE db file per box; entries exist with a NULL blob during the run,
    and consolidate() fills the blob in place — no second db, no patch file."""
    tmp = Path(tempfile.mkdtemp(prefix="ofl-"))
    _redirect_state(tmp)
    try:
        sid = "20260604-000000_111"
        backing = m.live_dir(sid); up = backing / "up"
        up.mkdir(parents=True)
        idx = m.Index(backing)
        wid = idx.writer_for(os.getpid())

        # FUSE-op simulation: bytes land in the upper, the row is upserted NULL-blob.
        (up / "newfile.txt").write_text("hello\nworld\n")
        idx.set_entry("newfile.txt", "file", stat_mod.S_IFREG | 0o644, wid, "create")
        (up / "blob.bin").write_bytes(bytes(range(256)))
        idx.set_entry("blob.bin", "file", stat_mod.S_IFREG | 0o644, wid, "create")
        os.symlink("/some/target", up / "lnk")
        idx.set_entry("lnk", "symlink", stat_mod.S_IFLNK | 0o777, wid, "symlink")
        assert Path("/etc/hostname").exists()
        idx.set_entry("etc/hostname", "whiteout", 0, wid, "unlink")

        sp = m.sqlar_path(sid)
        # ONE db file, at the single location.
        check(sp.exists(), "the single sqlar db exists at sqlar_path(sid)")
        check(not (backing / "index.db").exists(), "no separate index.db file")

        # during the run the file row exists with a NULL blob.
        conn = sqlite3.connect(str(sp))
        try:
            row = conn.execute("SELECT data FROM sqlar WHERE name='newfile.txt'").fetchone()
        finally: conn.close()
        check(row is not None and row[0] is None,
              "file entry exists with a NULL blob during the run")

        m.consolidate(str(backing), sid, ["echo", "hi"], index=idx)

        # after consolidate the SAME row's blob is populated.
        check(m.sqlar_content(sp, "newfile.txt") == b"hello\nworld\n",
              "consolidate filled the NULL blob with the actual contents")
        names = _names(sp)
        check("newfile.txt" in names and "blob.bin" in names and "lnk" in names,
              "all entries are in the one sqlar")
        check(m.sqlar_content(sp, "blob.bin") == bytes(range(256)),
              "binary content consolidated")
        tmode = m.sqlar_mode(sp, "etc/hostname")
        check(tmode is not None and stat_mod.S_ISCHR(tmode),
              "deletion is a char-device tombstone in the one db")
        # still exactly one db; no patch file anywhere.
        check(not list((tmp / "state" / "slopbox").glob("*.patch.xz")),
              "no patch.xz at rest")
        dbs = list((tmp / "state" / "slopbox").glob("*.sqlar"))
        check(len(dbs) == 1, f"exactly one db file at rest (got {len(dbs)})")

        # provenance carried into the same db.
        conn = sqlite3.connect(str(sp))
        try:
            rows = conn.execute("SELECT path,pid,argv FROM provenance").fetchall()
        finally: conn.close()
        provpaths = {r[0] for r in rows}
        check("blob.bin" in provpaths, "provenance recorded in the one db")
        idx.close()
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_process_and_env_tables_dedup_and_tag():
    """With tracing on, the process table records the writer tgid (deduped env),
    and the file entry's writer column points at that process row."""
    tmp = Path(tempfile.mkdtemp(prefix="ofl-"))
    _redirect_state(tmp)
    try:
        # sid pid == this process's tgid, so the recorded writer IS the box root
        # and the PPid-chain bubble-up stops at it: exactly one process row.
        sid = "20260604-000000_%d" % m.tgid_of(os.getpid())
        backing = m.live_dir(sid); (backing / "up").mkdir(parents=True)
        idx = m.Index(backing); idx.set_tracing(True)
        wid1 = idx.writer_for(os.getpid())
        wid2 = idx.writer_for(os.getpid())     # same tgid -> same row
        check(wid1 == wid2, "same tgid yields the same process row (deduped)")
        idx.set_entry("f.txt", "file", stat_mod.S_IFREG | 0o644, wid1, "create")

        sp = m.sqlar_path(sid)
        conn = sqlite3.connect(str(sp))
        try:
            nproc = conn.execute("SELECT COUNT(*) FROM process").fetchone()[0]
            nenv = conn.execute("SELECT COUNT(*) FROM env").fetchone()[0]
            wcol = conn.execute("SELECT writer FROM sqlar WHERE name='f.txt'").fetchone()[0]
            tgid = conn.execute("SELECT tgid FROM process WHERE id=?", (wcol,)).fetchone()[0]
        finally: conn.close()
        check(nproc == 1, "one process row for one tgid")
        check(nenv == 1, "one deduped env row")
        check(wcol == wid1, "file entry tagged with the producing process id")
        check(tgid == m.tgid_of(os.getpid()), "process row keyed by tgid")

        procs = m.process_list(sp)
        check(len(procs) == 1 and procs[0][0] == wid1, "process_list returns the row")
        env = m.process_env(sp, wid1)
        check(isinstance(env, dict) and len(env) > 0, "process env recorded when tracing")
        idx.close()
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_tracing_off_keeps_env_empty():
    tmp = Path(tempfile.mkdtemp(prefix="ofl-"))
    _redirect_state(tmp)
    try:
        sid = "20260604-000000_888"
        backing = m.live_dir(sid); (backing / "up").mkdir(parents=True)
        idx = m.Index(backing)             # tracing OFF
        wid = idx.writer_for(os.getpid())
        sp = m.sqlar_path(sid)
        check(m.process_env(sp, wid) == {}, "no env captured without -t")
        idx.close()
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_consolidate_opaque_expands_tombstones():
    tmp = Path(tempfile.mkdtemp(prefix="ofl-"))
    _redirect_state(tmp)
    try:
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
        idx.set_entry(rel, "dir", stat_mod.S_IFDIR | 0o755, wid, "mkdir", opaque=1)
        (up / rel / "kept.txt").write_text("x")
        idx.set_entry(rel + "/kept.txt", "file", stat_mod.S_IFREG | 0o644, wid, "create")

        m.consolidate(str(backing), sid, ["sh"], index=idx)
        sp = m.sqlar_path(sid)
        names = _names(sp)
        lower_children = [os.path.relpath(os.path.join(r, n), "/")
                          for r, ds, fs in os.walk(lower) for n in ds + fs]
        some_tombstoned = any(c in names and stat_mod.S_ISCHR(m.sqlar_mode(sp, c))
                              for c in lower_children)
        check(some_tombstoned, "opaque dir expanded into per-child tombstones")
        check(not stat_mod.S_ISCHR(m.sqlar_mode(sp, rel + "/kept.txt") or 0),
              "re-materialized child is NOT tombstoned")
        idx.close()
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_process_table_is_one_connected_tree():
    """A newly-recorded process bubbles its host PPid chain up to the box root so the
    process table forms ONE connected tree; an already-recorded process does not
    re-bubble; an init/unreadable parent terminates the walk without recording host
    system processes. Driven through a synthetic host chain (leaf->mid->root) so the
    walk is exercised deterministically — the real-process /proc path is covered by
    the other process-table tests."""
    tmp = Path(tempfile.mkdtemp(prefix="ofl-"))
    _redirect_state(tmp)
    ROOT, MID, LEAF = 1000, 1001, 1002        # host pids; ROOT goes in the sid
    chain = {LEAF: MID, MID: ROOT}            # child -> host PPid
    orig_rp, orig_tg = m.read_provenance, m.tgid_of
    m.tgid_of = lambda pid: int(pid or 0)     # these synthetic pids ARE tgids
    m.read_provenance = lambda pid, full_env=False: dict(
        ppid=chain.get(pid, 1), exe="/x/%d" % pid, argv=["p%d" % pid], env={})
    try:
        sid = "20260604-000000_%d" % ROOT
        backing = m.live_dir(sid); (backing / "up").mkdir(parents=True)
        idx = m.Index(backing); idx.set_tracing(True)
        # 3c records the root row at register; emulate it (root never bubbles).
        idx.process_from_prov(dict(tgid=ROOT, ppid=0, exe="", argv=["root"], env={}))
        # First op by the deepest process: record LEAF -> bubble LEAF->MID->ROOT.
        idx.process_from_prov(dict(tgid=LEAF, ppid=MID, exe="/x/leaf",
                                   argv=["leaf"], env={}))

        sp = m.sqlar_path(sid)
        by_tgid = {tgid: ppid for _rid, tgid, ppid, _exe, _argv in m.process_list(sp)}
        for p in (ROOT, MID, LEAF):
            check(p in by_tgid, "level pid %d has a process row" % p)
        # connectivity: each non-root row's ppid points at another recorded row.
        for tgid, ppid in by_tgid.items():
            if tgid != ROOT:
                check(ppid in by_tgid,
                      "row tgid=%d ppid=%d points at a recorded row" % (tgid, ppid))
        # exactly one root: the only row whose ppid is not itself recorded.
        roots = [t for t, ppid in by_tgid.items() if ppid not in by_tgid]
        check(roots == [ROOT], "the sole dangling parent is the box root")

        # idempotent: re-recording a known process adds no rows and no new bubbling.
        n_before = len(by_tgid)
        idx.process_from_prov(dict(tgid=LEAF, ppid=MID, exe="", argv=[], env={}))
        check(len(m.process_list(sp)) == n_before,
              "re-recording a known process does not re-bubble")

        # init parent terminates the walk: a process whose ppid is pid 1 records
        # ONLY itself, never pid 0/1 or host system procs.
        idx.process_from_prov(dict(tgid=0x7fffffff, ppid=1, exe="", argv=[], env={}))
        tgids = {t for _r, t, _p, _e, _a in m.process_list(sp)}
        check(0x7fffffff in tgids, "the pid-1-parented process is recorded")
        check(0 not in tgids and 1 not in tgids,
              "init (pid 0/1) is never recorded — walk stops at the boundary")
        idx.close()
    finally:
        m.read_provenance, m.tgid_of = orig_rp, orig_tg
        shutil.rmtree(tmp, ignore_errors=True)


def test_supervisor_no_mount_register_fails_closed():
    sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=None)
    ack = sup.register(dict(session_id="20260604-000000_111", cmd=["true"]))
    check(ack.get("ok") is False, "register without a mount fails closed (no ok)")
    check("error" in ack, "register failure carries an error message")


def test_box_id_and_pool_layout():
    """The box's stable pool id is minted once, unique per box, persisted in the
    sqlar's own meta (so it survives the sqlar being renamed), and the blob path is
    addressed by <box_id>/<shard>/<row_id> — never by host path."""
    tmp = Path(tempfile.mkdtemp(prefix="pool-"))
    _redirect_state(tmp)
    try:
        a = "20260604-000000_1"; b = "20260604-000000_2"
        # mint sqlars (touch a meta entry so the files exist)
        m.sqlar_meta_set(m.sqlar_path(a), "born", a)
        m.sqlar_meta_set(m.sqlar_path(b), "born", b)
        ida = m.ensure_box_id(a); idb = m.ensure_box_id(b)
        check(isinstance(ida, int) and isinstance(idb, int), "box ids are ints")
        check(ida != idb, "distinct boxes get distinct pool ids")
        check(m.ensure_box_id(a) == ida, "box id is stable across calls (minted once)")
        # rename the sqlar file: the id travels with it (lives in meta, not the name)
        renamed = m.sqlar_path("RENAMED")
        m.sqlar_path(a).rename(renamed)
        check(m.ensure_box_id("RENAMED") == ida,
              "box id survives a sqlar rename (stored in meta, not the filename)")
        # blob path layout: <pool>/<box_id>/<shard>/<row_id>, shard = row % SHARDS
        bp = m.blob_path(ida, 1234)
        check(bp.parent.parent == m.box_pool_dir(ida),
              "blob path is rooted at the box's pool dir")
        check(bp.name == "1234" and bp.parent.name == f"{1234 % m.POOL_SHARDS:03x}",
              "blob path is <box_id>/<shard>/<row_id>")
        # orphan sweep: a pool dir with no surviving sqlar id is removed; a live one stays
        m.box_pool_dir(idb).mkdir(parents=True, exist_ok=True)
        ghost = m.box_pool_dir(999999); ghost.mkdir(parents=True, exist_ok=True)
        m.sweep_orphan_pools()
        check(m.box_pool_dir(idb).exists(), "pool dir of a live box is kept")
        check(not ghost.exists(), "pool dir with no surviving box is swept")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    for t in (test_one_db_only_and_blob_lifecycle,
              test_process_and_env_tables_dedup_and_tag,
              test_tracing_off_keeps_env_empty,
              test_consolidate_opaque_expands_tombstones,
              test_process_table_is_one_connected_tree,
              test_supervisor_no_mount_register_fails_closed,
              test_box_id_and_pool_layout):
        print(f"\n== {t.__name__} ==")
        try:
            t()
        except Exception as e:
            import traceback; traceback.print_exc(); _fails.append(f"{t.__name__}: {e}")
    print("\n" + ("ALL PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    sys.exit(1 if _fails else 0)
