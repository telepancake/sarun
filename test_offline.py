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

        # FUSE-op simulation: bytes land in the pool blob, the row is upserted NULL-blob.
        idx.set_entry("newfile.txt", "file", stat_mod.S_IFREG | 0o644, wid, "create")
        bp = m.blob_path(idx.box_id, idx.row_id("newfile.txt"))
        bp.parent.mkdir(parents=True, exist_ok=True); bp.write_bytes(b"hello\nworld\n")
        idx.set_entry("blob.bin", "file", stat_mod.S_IFREG | 0o644, wid, "create")
        bp = m.blob_path(idx.box_id, idx.row_id("blob.bin"))
        bp.parent.mkdir(parents=True, exist_ok=True); bp.write_bytes(bytes(range(256)))
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
        # sid pid == this process's tgid, so the recorded writer IS the box root.
        # The PPid-bubbling boundary is the explicitly-registered root (process.root
        # column), exactly as the real register flow records it — so register our own
        # tgid as the root first; the bubble-up then stops at it: one process row.
        sid = "20260604-000000_%d" % m.tgid_of(os.getpid())
        backing = m.live_dir(sid); (backing / "up").mkdir(parents=True)
        idx = m.Index(backing); idx.set_tracing(True)
        root_prov = m.read_provenance(os.getpid(), full_env=True)
        idx.process_from_prov(dict(tgid=m.tgid_of(os.getpid()),
                                   ppid=m.tgid_of(root_prov["ppid"]) if root_prov.get("ppid") else 0,
                                   exe=root_prov["exe"], argv=root_prov["argv"],
                                   env=root_prov.get("env") or {}), root=True)
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
        idx.set_entry(rel + "/kept.txt", "file", stat_mod.S_IFREG | 0o644, wid, "create")
        bp = m.blob_path(idx.box_id, idx.row_id(rel + "/kept.txt"))
        bp.parent.mkdir(parents=True, exist_ok=True); bp.write_bytes(b"x")
        # the dir must also exist in up/ for the opaque expansion check
        (up / rel).mkdir(parents=True, exist_ok=True)

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


def test_pidfd_alive():
    """_pidfd_alive() is True for a running process's pidfd and False after it exits.
    Uses a real child process so the fd is a genuine pidfd, not just any fd.
    Wrap-immune: the pidfd names one exact process incarnation."""
    import subprocess, time
    try:
        os.pidfd_open   # guard: skip if unavailable (kernel < 5.3)
    except AttributeError:
        print("  skip  test_pidfd_alive: os.pidfd_open unavailable")
        return
    # Negative / invalid fd → False immediately.
    check(m._pidfd_alive(-1) is False, "pidfd_alive: -1 → False")
    check(m._pidfd_alive(None) is False, "pidfd_alive: None → False")
    # Live child → True.
    p = subprocess.Popen(["sleep", "30"])
    fd = os.pidfd_open(p.pid)
    try:
        check(m._pidfd_alive(fd) is True, "pidfd_alive: live child → True")
        p.terminate()
        p.wait()
        # The pidfd becomes readable once the process exits; poll briefly.
        alive = True
        for _ in range(20):
            if not m._pidfd_alive(fd):
                alive = False
                break
            time.sleep(0.01)
        check(not alive, "pidfd_alive: terminated child → False")
    finally:
        os.close(fd)
        if p.poll() is None:
            p.kill(); p.wait()


def test_consolidate_promote_to_parent_sqlar():
    """_promote_to_parent_sqlar and the consolidate parent path: call consolidate()
    directly with a monkey-patched load_file_rules that returns an 'apply' rule so
    the pool-blob file is promoted into the parent sqlar instead of the real host."""
    tmp = Path(tempfile.mkdtemp(prefix="ofl-promo-"))
    _redirect_state(tmp)
    orig_load = m.load_file_rules
    try:
        parent_sid = "20260604-000200_300"
        child_sid  = "20260604-000200_301"

        # Set up child's overlay.
        backing = m.live_dir(child_sid); up = backing / "up"
        up.mkdir(parents=True)
        idx = m.Index(backing)
        wid = idx.writer_for(os.getpid())
        rel = "tmp/promo_file.txt"
        content = b"promoted content\n"
        idx.set_entry(rel, "file", stat_mod.S_IFREG | 0o644, wid, "create")
        bp = m.blob_path(idx.box_id, idx.row_id(rel))
        bp.parent.mkdir(parents=True, exist_ok=True)
        bp.write_bytes(content)

        # Ensure the parent's sqlar exists (empty schema).
        p_sp = m.sqlar_path(parent_sid)
        m._sqlar_open(p_sp).close()

        # Monkey-patch load_file_rules to return an 'apply' rule for our rel.
        class _ApplyRule:
            def decide(self, r): return "apply" if r == rel else None
        m.load_file_rules = lambda: _ApplyRule()

        # Run consolidate with parent set.
        m.consolidate(str(backing), child_sid, ["sh"], index=idx,
                      parent=parent_sid)

        # The parent's sqlar should carry the promoted entry.
        rows = m.sqlar_list(p_sp)
        names = {r[0] for r in rows}
        check(rel in names,
              f"consolidate promote: parent sqlar has {rel!r}")
        pmode = m.sqlar_mode(p_sp, rel)
        check(pmode is not None and stat_mod.S_ISREG(pmode),
              "consolidate promote: entry mode is regular-file")
        check(m.sqlar_content(p_sp, rel) == content,
              "consolidate promote: parent sqlar content matches child bytes")

        # The child's sqlar should NOT have the row (it was dropped, not stored).
        c_sp = m.sqlar_path(child_sid)
        c_names = _names(c_sp)
        check(rel not in c_names,
              "consolidate promote: child sqlar has no row for the promoted path")

        # The real host must NOT have been written.
        host_path = Path("/") / rel
        check(not host_path.exists(),
              "consolidate promote: real host not written (/tmp/promo_file.txt absent)")

        idx.close()
    finally:
        m.load_file_rules = orig_load
        shutil.rmtree(tmp, ignore_errors=True)


def test_consolidate_root_box_apply_still_writes_host():
    """consolidate() with parent=None and an 'apply' rule writes the real host — the
    original behaviour is unchanged for root boxes."""
    tmp = Path(tempfile.mkdtemp(prefix="ofl-root-apply-"))
    _redirect_state(tmp)
    orig_load = m.load_file_rules
    # Use a path we can actually write; /tmp is always writable.
    host_rel = "tmp/root_apply_test_sarun_offline.txt"
    host_path = Path("/") / host_rel
    try:
        host_path.unlink(missing_ok=True)

        sid = "20260604-000200_302"
        backing = m.live_dir(sid); up = backing / "up"
        up.mkdir(parents=True)
        idx = m.Index(backing)
        wid = idx.writer_for(os.getpid())
        content = b"root apply test\n"
        idx.set_entry(host_rel, "file", stat_mod.S_IFREG | 0o644, wid, "create")
        bp = m.blob_path(idx.box_id, idx.row_id(host_rel))
        bp.parent.mkdir(parents=True, exist_ok=True)
        bp.write_bytes(content)

        # Monkey-patch load_file_rules to apply this specific path.
        class _ApplyRule:
            def decide(self, r): return "apply" if r == host_rel else None
        m.load_file_rules = lambda: _ApplyRule()

        # parent=None → host write.
        m.consolidate(str(backing), sid, ["sh"], index=idx, parent=None)

        check(host_path.exists(),
              "root-box apply: real host was written")
        if host_path.exists():
            check(host_path.read_bytes() == content,
                  "root-box apply: host content matches")
        idx.close()
    finally:
        m.load_file_rules = orig_load
        host_path.unlink(missing_ok=True)
        shutil.rmtree(tmp, ignore_errors=True)


def test_promote_into_parent_unit():
    """Unit-test _promote_into_parent directly: a live parent Index receives the
    promotion for all three plan kinds (file / symlink / delete)."""
    import stat as stat_mod
    tmp = Path(tempfile.mkdtemp(prefix="ofl-unit-promo-"))
    _redirect_state(tmp)
    try:
        class _FakeSessions:
            def get(self, k, d=None): return self._s.get(k, d)
            def __contains__(self, k): return k in self._s
            def __getitem__(self, k): return self._s[k]
            def __init__(self): self._s = {}

        class _FakeReg:
            def __init__(self):
                self.sessions = _FakeSessions()
                self.indexes = {}

        class _FakeReview:
            def __init__(self): self.reg = _FakeReg()

        rev = _FakeReview()

        # Set up a live parent index.
        parent_sid = "20260604-000201_400"
        p_backing = m.live_dir(parent_sid); p_up = p_backing / "up"
        p_up.mkdir(parents=True)
        p_idx = m.Index(p_backing)
        rev.reg.indexes[parent_sid] = p_idx

        # Wire a minimal Session so _parent_sid / _promote_into_parent can find upper.
        ps = m.Session(session_id=parent_sid, cmd=["p"], shm_dir=str(p_backing), live=True)
        rev.reg.sessions._s[parent_sid] = ps

        # Build the real ChangeReview, monkey-patch its reg.
        cr = m.ChangeReview(rev.reg)

        # ── file plan ──
        rel_f = "usr/lib/promo.so"
        content = b"\x7fELF\x00"
        plan_f = dict(kind="file", data=content, chmod=stat_mod.S_IFREG | 0o644)
        err = cr._promote_into_parent(parent_sid, rel_f, plan_f)
        check(err is None, "unit-promo file: no error returned")
        check(p_idx.kind_of(rel_f) == "file", "unit-promo file: parent Index kind=file")
        p_rid = p_idx.row_id(rel_f)
        check(p_rid is not None, "unit-promo file: parent Index has row_id")
        bp = m.blob_path(p_idx.box_id, p_rid)
        check(bp.exists() and bp.read_bytes() == content,
              "unit-promo file: blob content correct")

        # ── symlink plan ──
        rel_l = "etc/resolv.conf"
        plan_l = dict(kind="symlink", target="/run/systemd/resolve/stub-resolv.conf")
        err = cr._promote_into_parent(parent_sid, rel_l, plan_l)
        check(err is None, "unit-promo symlink: no error")
        check(p_idx.kind_of(rel_l) == "symlink", "unit-promo symlink: parent kind=symlink")
        sym = p_up / rel_l
        check(sym.is_symlink(), "unit-promo symlink: symlink artifact in parent up/")

        # ── delete plan (path absent on host → del_entry, not whiteout) ──
        rel_d = "tmp/nonexistent_promo_path_test"
        plan_d = dict(kind="delete")
        err = cr._promote_into_parent(parent_sid, rel_d, plan_d)
        check(err is None, "unit-promo delete: no error")
        # Either whiteout or del_entry depending on host presence; both are fine.
        # We just confirm no crash and no host write.
        host_d = Path("/") / rel_d
        check(not host_d.exists(), "unit-promo delete: host not touched")

        p_idx.close()
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_consolidate_size_based_placement():
    """consolidate() chooses a per-file rest form by size: a small file folds into the
    sqlar blob (data NOT NULL, pool file evicted); a large file (>= POOL_RESIDENT_MIN)
    stays a PERMANENT pool file (row resident: data NULL, blob kept). sqlar_content
    and the SqlarArchive readers serve BOTH forms, and delete() removes the pool dir."""
    tmp = Path(tempfile.mkdtemp(prefix="ofl-"))
    _redirect_state(tmp)
    try:
        sid = "20260604-000000_222"
        backing = m.live_dir(sid); up = backing / "up"; up.mkdir(parents=True)
        idx = m.Index(backing)
        wid = idx.writer_for(os.getpid())

        small = b"a tiny config line\n"
        big = (b"X" * 7) * (m.POOL_RESIDENT_MIN // 7 + 1)   # > threshold
        check(len(big) >= m.POOL_RESIDENT_MIN, "big file is at/above the resident threshold")
        for rel, payload in (("etc/small.conf", small), ("var/big.bin", big)):
            idx.set_entry(rel, "file", stat_mod.S_IFREG | 0o644, wid, "create")
            bp = m.blob_path(idx.box_id, idx.row_id(rel))
            bp.parent.mkdir(parents=True, exist_ok=True); bp.write_bytes(payload)

        small_bp = m.blob_path(idx.box_id, idx.row_id("etc/small.conf"))
        big_bp = m.blob_path(idx.box_id, idx.row_id("var/big.bin"))
        box_id = idx.box_id
        sp = m.sqlar_path(sid)

        m.consolidate(str(backing), sid, ["sh"], index=idx)

        # ── small file: folded into the row, pool file gone ─────────────────────
        conn = sqlite3.connect(str(sp))
        try:
            srow = conn.execute("SELECT sz,data FROM sqlar WHERE name='etc/small.conf'").fetchone()
            brow = conn.execute("SELECT sz,data FROM sqlar WHERE name='var/big.bin'").fetchone()
        finally: conn.close()
        check(srow is not None and srow[1] is not None,
              "small file folded into the sqlar blob (data NOT NULL)")
        check(not small_bp.exists(), "small file's pool blob was evicted")

        # ── large file: stays a permanent pool file (resident row) ──────────────
        check(brow is not None and brow[1] is None,
              "large file row stays resident (data IS NULL)")
        check(brow[0] == len(big),
              f"resident row records real uncompressed size (got {brow[0]}, want {len(big)})")
        check(big_bp.exists(), "large file's pool blob is KEPT as the rest form")

        # ── both rest forms read back correctly through sqlar_content ────────────
        check(m.sqlar_content(sp, "etc/small.conf") == small,
              "sqlar_content serves the folded (evicted) small file")
        check(m.sqlar_content(sp, "var/big.bin") == big,
              "sqlar_content serves the resident large file from its pool blob")

        # ── SqlarArchive.entries() reports the real size for the resident row ────
        # Build a minimal Supervisor just to host a ChangeReview / SqlarArchive.
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=None)
        arch = m.SqlarArchive(sup.review, sid)
        ent = {e["path"]: e for e in arch.entries()}
        check(ent.get("var/big.bin", {}).get("size") == len(big),
              "SqlarArchive.entries() reports the resident file's real size")
        check(arch.current_bytes("var/big.bin") == big,
              "SqlarArchive.current_bytes serves the resident large file")
        check(arch.apply_plan("var/big.bin").get("data") == big,
              "SqlarArchive.apply_plan carries the resident file's bytes")

        # ── delete() removes the box's pool dir (no leaked permanent blobs) ──────
        pool_dir = m.box_pool_dir(box_id)
        check(pool_dir.exists(), "pool dir present before delete")
        idx.close()
        sup.sessions[sid] = m.Session(session_id=sid, cmd=["sh"], live=False,
                                      shm_dir=str(backing))
        sup.delete(sid)
        check(not pool_dir.exists(), "delete() removed the box's pool dir")
        check(not sp.exists(), "delete() removed the sqlar")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    for t in (test_one_db_only_and_blob_lifecycle,
              test_process_and_env_tables_dedup_and_tag,
              test_tracing_off_keeps_env_empty,
              test_consolidate_opaque_expands_tombstones,
              test_process_table_is_one_connected_tree,
              test_supervisor_no_mount_register_fails_closed,
              test_pidfd_alive,
              test_box_id_and_pool_layout,
              test_consolidate_promote_to_parent_sqlar,
              test_consolidate_root_box_apply_still_writes_host,
              test_consolidate_size_based_placement,
              test_promote_into_parent_unit):
        print(f"\n== {t.__name__} ==")
        try:
            t()
        except Exception as e:
            import traceback; traceback.print_exc(); _fails.append(f"{t.__name__}: {e}")
    print("\n" + ("ALL PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    sys.exit(1 if _fails else 0)
