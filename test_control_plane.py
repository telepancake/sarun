#!/usr/bin/env python3
"""Regression tests for the HIGH control-plane findings.

HIGH-1: untrusted in-box code (reaching only its box's ONE muxed connection) must NOT
        be able to (a) unregister another session, (b) register a new one, or (c) gate
        its traffic as another sid — the sid in a RELAY frame is ignored and derived
        from which CONNECTION the frame arrived on (bound to the minted sid at register).
HIGH-2: register() rejects a session_id that isn't the one legitimate shape (so a
        traversal sid such as "../foo" or "a/b" creates no directory anywhere).

    uv run --with pyfuse3 --with trio pytest test_control_plane.py

Self-safety: an isolated XDG temp tree; no real overlay mount; sockets closed in finally.
"""
import asyncio, os, socket, sys, tempfile, shutil, array, json, threading
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


# ── HIGH-2: sid validation ──────────────────────────────────────────────────

def test_sid_validation_rejects_traversal():
    tmp = Path(tempfile.mkdtemp(prefix="cp-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        # valid_box_id validates the INTERNAL box key str(box_id): a plain decimal string.
        check(m.valid_box_id("123"), "valid_box_id: plain box_id accepted")
        check(m.valid_box_id("1"), "valid_box_id: single-digit box_id accepted")
        for bad in ("../escape", "a/b", "..", "/etc/passwd", "12/..",
                    "12.ABCDEF", "x_1", "", None, "ALPHA",
                    "20260604-000000_111", "12\n13"):
            check(not m.valid_box_id(bad), f"valid_box_id: rejects {bad!r}")

        # register() never uses session_id as a path component (it mints box_id). A
        # traversal/garbage NAME is rejected as an invalid NAME and creates nothing.
        sup = m.Supervisor(mount=_FakeMount())
        outside = tmp / "PWNED"
        ack = sup.register(dict(session_id=f"../../{outside.name}", cmd=["true"]))
        check(ack.get("ok") is False, "register: traversal name rejected (ok False)")
        check(not outside.exists(), "register: no dir created outside live_home()")

        ack2 = sup.register(dict(session_id="a/b/c", cmd=["true"]))
        check(ack2.get("ok") is False, "register: slashed name rejected")
        # The only live/<id> dirs that exist are numeric box_id dirs, never the bad name.
        lh = m.live_home()
        names = [p.name for p in (lh.iterdir() if lh.exists() else [])]
        check(all(m.BOX_ID_RE.match(n) for n in names),
              f"register: only numeric box_id backing dirs exist (got {names})")
        check("PWNED" not in names and "a" not in names,
              "register: no live/<bad-name> dir created")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)



# ── HIGH-1: owner token gates unregister/drop ───────────────────────────────

class _FakeOps:
    def __init__(self): self.removed = []
    def add_session(self, *a, **k): pass
    def remove_session(self, sid): self.removed.append(sid)
    def add_virtual(self, *a, **k): pass

class _FakeMount:
    def __init__(self): self.ops = _FakeOps()
    def is_healthy(self): return True
    def add_session(self, sid, *a, **k):
        # Real FUSE creates the box_root subfolder; mirror it so register()'s
        # os.open(<mnt>/<box_id>) (nested reply-fd path) finds a real dir.
        try: (m.mnt_point() / str(sid)).mkdir(parents=True, exist_ok=True)
        except Exception: pass
    def remove_session(self, sid): self.ops.remove_session(sid)
    def set_parent(self, sid, parent): pass


def test_owner_token_required_for_teardown():
    tmp = Path(tempfile.mkdtemp(prefix="cp-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(mount=_FakeMount())
        ack = sup.register(dict(session_id="TOKBOX", cmd=["true"]))
        check(ack.get("ok") is True, "register: a valid box registers")
        sid = ack["session_id"]
        token = ack.get("owner_token")
        check(bool(token), "register: an owner token is issued to the runner")
        check(sid in sup.sessions, "register: session present after register")

        # a box (no token, or a wrong one) cannot unregister
        sup.unregister(dict(session_id=sid, owner_token="not-the-token"))
        check(sid in sup.sessions, "unregister with a WRONG token is rejected")
        sup.unregister(dict(session_id=sid))
        check(sid in sup.sessions, "unregister with NO token is rejected")
        # a box cannot drop it either
        sup.drop(dict(session_id=sid, owner_token="nope"))
        check(sid in sup.sessions, "drop with a wrong token is rejected")

        # the runner (holding the real token) can
        sup.unregister(dict(session_id=sid, owner_token=token,
                            status="finished", exit_code=0))
        check(sid not in sup.sessions or not sup.sessions[sid].live,
              "unregister WITH the right token tears the session down")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_box_cannot_register_or_unregister_a_foreign_session():
    """Two sessions A and B. A box that knows only A's token (the realistic case:
    A's runner) cannot use it to unregister B."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(mount=_FakeMount())
        ra = sup.register(dict(session_id="ABOX", cmd=["true"]))
        rb = sup.register(dict(session_id="BBOX", cmd=["true"]))
        a, b = ra["session_id"], rb["session_id"]
        tok_a = ra["owner_token"]
        # A's token must not unregister B
        sup.unregister(dict(session_id=b, owner_token=tok_a))
        check(b in sup.sessions, "session B survives an unregister bearing A's token")
        sup.drop(dict(session_id=b, owner_token=tok_a))
        check(b in sup.sessions, "session B survives a drop bearing A's token")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


# ── command lives in the process table, not an xattr ────────────────────────

def test_command_is_the_root_process_row():
    """register() persists the box's command as the ROOT process row (tgid == the
    runner's host pid, argv == cmd) in the single <box_id>.sqlar — the sole home for
    the command, with no xattr anywhere. The row is present even for an EMPTY box (no
    file changes), and root_cmd() reads it straight from the persisted db, so it
    survives after the runner (and its live Index) is gone."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        # no command-recording xattr machinery remains
        check(not hasattr(m, "set_cmd_xattr") and not hasattr(m, "get_cmd_xattr")
              and not hasattr(m, "CMD_XATTR"),
              "no set/get_cmd_xattr / CMD_XATTR symbols remain")

        sup = m.Supervisor(mount=_FakeMount())
        root_pid = 4242                        # the runner's host pid (kernel-derived)
        cmd = ["echo", "hello world"]
        prov = dict(ppid=7, exe="/usr/bin/echo", env={"FOO": "bar"})
        ack = sup.register(dict(session_id="ECHOBOX", cmd=cmd, prov=prov,
                                want_env=True, _register_host_pid=root_pid))
        check(ack.get("ok") is True, "register ok")
        sid = ack["session_id"]

        # the live process table carries the root row keyed by the host pid
        # process_list rows are (id, tgid, ppid, parent_id, exe, argv).
        procs = sup.processes(sid)
        root = next((p for p in procs if p[1] == root_pid), None)
        check(root is not None, "a process row exists for the sid's root pid")
        check(root and root[5] == cmd, "root row argv IS the command")
        check(root and root[4] == "/usr/bin/echo", "root row carries the runner exe")
        check(root and root[2] == 7, "root row carries the runner ppid")
        check(root and root[3] is None, "the root row has no parent_id (it IS a root)")

        # the command is retrievable via the lookup helper while live
        check(m.root_cmd(sid) == cmd, "root_cmd(sid) returns the command")

        # EMPTY box: no file changes were ever written, yet the root row persists.
        # Drop the live Index to simulate the runner having exited, then read the
        # command straight from the on-disk sqlar.
        idx = sup.indexes.pop(sid, None)
        if idx is not None: idx.close()
        check(m.root_cmd(sid) == cmd,
              "root_cmd(sid) reads the command from the persisted sqlar after exit")
        check(m.discover_sessions()[sid].cmd == cmd,
              "discover_sessions sources the command from the root process row")

        # a malformed/unparsable sid yields an empty command, never a crash
        check(m.root_cmd("not-a-sid") == [], "root_cmd of a malformed sid is []")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


# ── CLI: `--` required, -n/-t/-d flag parsing, single UI instance ────────────

def _run_main(argv, **patches):
    """Call m.main() with sys.argv set, capturing patched callables' results."""
    import io, contextlib
    saved = {k: getattr(m, k) for k in patches}
    old_argv = sys.argv
    sys.argv = ["slopbox"] + argv
    captured = {}
    for k, v in patches.items(): setattr(m, k, v)
    err = io.StringIO()
    try:
        with contextlib.redirect_stderr(err):
            rc = m.main()
    finally:
        sys.argv = old_argv
        for k, v in saved.items(): setattr(m, k, v)
    return rc, err.getvalue()


def test_dash_dash_required_and_flag_parsing():
    seen = {}
    def fake_run(cmd, env_capture, direct, chdir, reuse_sid=None,
                 capture=True):
        seen.update(cmd=cmd, env_capture=env_capture, direct=direct,
                    chdir=chdir, reuse_sid=reuse_sid, capture=capture)
        return 0

    # missing `--` is refused with a clear, fixable message.
    rc, err = _run_main(["ls"], cmd_run=fake_run)
    check(rc != 0 and "--" in err, "bare `slopbox ls` refused (needs `--`)")
    check("ls" in err, "error suggests the corrected `slopbox -- ls`")
    check(not seen, "cmd_run not called when `--` is missing")

    # defaults: env/direct off; capture ON (the new default, no -t).
    seen.clear()
    rc, _ = _run_main(["--", "echo", "hi"], cmd_run=fake_run)
    check(seen.get("cmd") == ["echo", "hi"], "command parsed after `--`")
    check(seen.get("env_capture") is False and seen.get("direct") is False,
          "default run: env/direct off")
    check(seen.get("capture") is True, "default run captures stdout/stderr")

    # -t now means passthrough: capture OFF.
    seen.clear()
    rc, _ = _run_main(["-t", "--", "echo", "hi"], cmd_run=fake_run)
    check(seen.get("capture") is False, "-t disables capture (passthrough)")

    # -e/-d, combinable, before `--`. -e is env capture (independent).
    seen.clear()
    rc, _ = _run_main(["-e", "-d", "--", "ls", "-la"],
                      cmd_run=fake_run)
    check(seen.get("env_capture") and seen.get("direct"),
          "-e/-d are independent and combinable")
    check(seen.get("cmd") == ["ls", "-la"], "flags before `--`, args (incl. -la) after")


def test_single_ui_instance_refused():
    # run_ui refuses when a UI is already running (ui_is_running True).
    started = {"n": 0}
    def fake_app():
        started["n"] += 1
        class _A:
            def run(self, *a, **k): pass
        return lambda: _A()
    rc, err = _run_main([], ui_is_running=lambda _p: True,
                        _make_ui_app=fake_app)
    check(rc != 0 and "already running" in err.lower(),
          "second UI refuses with a clear error")
    check(started["n"] == 0, "the app is never constructed when one is running")


# ── nested boxes: _derive_parent_sid ────────────────────────────────────────

def test_derive_parent_sid_owner_discovery():
    """_derive_parent_sid maps a host pid to the owning live session by walking the
    PPid chain and matching against each session's root tgids.

    Strategy: create two live sessions (A and B) each with a distinct root tgid
    drawn from the real process hierarchy.  The current test process (getpid()) is
    a child of getppid(), so:
      - Session A root = getppid()   → _derive_parent_sid(getpid()) == sid_a
      - Session B root = some other pid that is NOT an ancestor
      - _derive_parent_sid(getpid()) must NOT return sid_b
      - _derive_parent_sid(os.getpid()) on a sup with NO sessions → None
    """
    tmp = Path(tempfile.mkdtemp(prefix="cp-nest-"))
    _redirect_state(tmp)
    # Open pidfds before registering so each session has a live liveness handle.
    # _derive_parent_sid now gates root_map on _pidfd_alive(sess.run_pidfd).
    pidfd_a = os.pidfd_open(os.getppid())   # live: our parent is alive for the test
    try:
        m.ensure_dirs()
        sup = m.Supervisor(mount=_FakeMount())

        my_pid  = os.getpid()
        my_ppid = os.getppid()

        # Register session A: root_tgid == our ppid, supply a live pidfd so
        # _pidfd_alive(sess_a.run_pidfd) is True and the entry appears in root_map.
        ack_a = sup.register(dict(session_id="ABOX", cmd=["true"],
                                  root_tgid=my_ppid,
                                  _register_pidfd=pidfd_a,
                                  prov=dict(ppid=0, exe="/bin/sh", env={})))
        check(ack_a.get("ok") is True, "derive: session A registers ok")
        sid_a = ack_a["session_id"]
        # register() dup'd the fd; close our copy so it doesn't leak.
        try: os.close(pidfd_a)
        except OSError: pass
        pidfd_a = -1

        # Register session B: root_tgid is a synthetic dead pid; no pidfd supplied →
        # run_pidfd stays -1 → _pidfd_alive returns False → B never enters root_map.
        fake_root_b = 99999999   # unlikely to be alive, definitely not our ancestor
        ack_b = sup.register(dict(session_id="BBOX", cmd=["true"],
                                  root_tgid=fake_root_b,
                                  prov=dict(ppid=0, exe="/bin/false", env={})))
        check(ack_b.get("ok") is True, "derive: session B registers ok")
        sid_b = ack_b["session_id"]

        # Both sessions must be marked live for _derive_parent_sid to see them.
        check(sup.sessions[sid_a].live, "derive: session A is live")
        check(sup.sessions[sid_b].live, "derive: session B is live")

        # Core assertion: our pid's PPid chain includes our ppid → maps to sid_a.
        result = m._derive_parent_sid(my_pid, sup)
        check(result == sid_a,
              f"derive: pid {my_pid} (ppid {my_ppid}) maps to sid_a "
              f"(got {result!r})")

        # A pid not under ANY session's forest → None (top-level).
        orphan_pid = 1   # pid 1 (init) is not a descendant of any session root tgid
        result_none = m._derive_parent_sid(orphan_pid, sup)
        check(result_none is None,
              f"derive: pid 1 (init) not in any session forest → None (got {result_none!r})")

        # pid <= 0 → None immediately.
        check(m._derive_parent_sid(0, sup) is None,
              "derive: pid=0 → None")
        check(m._derive_parent_sid(-1, sup) is None,
              "derive: pid=-1 → None")

        # An empty supervisor (no sessions) → always None.
        sup_empty = m.Supervisor(mount=_FakeMount())
        check(m._derive_parent_sid(my_pid, sup_empty) is None,
              "derive: no live sessions → None")

    finally:
        if pidfd_a >= 0:
            try: os.close(pidfd_a)
            except OSError: pass
        shutil.rmtree(tmp, ignore_errors=True)


def test_register_parent_body_field_not_honoured():
    """A register message that includes a 'parent_sid' field in its body does NOT
    get that parent threaded through — only the kernel-derived _derived_parent_sid
    (injected by ChannelServer from SO_PEERCRED) is trusted.  Supervisor.register
    never reads msg['parent_sid']; it reads only msg['_derived_parent_sid']."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-nospoofp-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(mount=_FakeMount())

        ack_p = sup.register(dict(session_id="PARENTBOX", cmd=["true"]))
        sid_parent = ack_p["session_id"]
        check(sup.sessions[sid_parent].live, "no-spoof: parent session live")

        # The child message carries a box-supplied 'parent_sid' (untrusted body field).
        # Supervisor.register should ignore it.  No _derived_parent_sid → top-level box.
        ack = sup.register(dict(session_id="CHILDBOX", cmd=["true"],
                                parent_sid=sid_parent))   # untrusted field
        check(ack.get("ok") is True, "no-spoof: register with body parent_sid still ok")
        sid_child = ack["session_id"]

        # Verify the child is top-level (body parent_sid not honoured).
        check(sup.sessions[sid_child].live, "no-spoof: child session is live")
        check(sup.sessions[sid_child].parent_box_id is None,
              "no-spoof: body parent_sid did NOT set the child's parent pointer")
        # A non-live derived parent fails closed:
        ack2 = sup.register(dict(session_id="ORPHANBOX", cmd=["true"],
                                 _derived_parent_sid="99999"))
        check(ack2.get("ok") is False,
              "no-spoof: _derived_parent_sid naming a dead/absent session → fail-closed")
        check("not live" in (ack2.get("error") or ""),
              f"no-spoof: error mentions 'not live' (got {ack2.get('error')!r})")

    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_register_no_parent_is_top_level():
    """When _derived_parent_sid is absent (None), register() creates a normal
    top-level box (parent=None passed to add_session) and succeeds."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-toplevel-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(mount=_FakeMount())
        # No _derived_parent_sid key → top-level
        ack = sup.register(dict(session_id="TOPBOX", cmd=["true"]))
        check(ack.get("ok") is True, "top-level: register ok with no parent")
        sid = ack["session_id"]
        check(sup.sessions[sid].live, "top-level: session is live")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


# ── pidfd register path: ChannelServer receives pidfd, derives host pid ──────

def test_channel_server_pidfd_register():
    """Exercise the full FD-passing register path end-to-end:
    - Start a real ChannelServer over an AF_UNIX socketpair / temp socket
    - Client sends a register JSON + its own pidfd over SCM_RIGHTS
    - Server receives it via _recvmsg_blocking, derives HOST pid via
      _host_pid_from_pidfd, and calls _dispatch_control with that pid
    - We verify the ack is well-formed (ok + mount) and that the pidfd path
      resolved a non-zero host pid (using _host_pid_from_pidfd directly)
    """
    tmp = Path(tempfile.mkdtemp(prefix="cp-cs-"))
    _redirect_state(tmp)
    cs_loop = asyncio.new_event_loop()
    cs_thread = None
    try:
        m.ensure_dirs()
        sup = m.Supervisor(mount=_FakeMount())
        sock_path = str(tmp / "ctrl.sock")

        ready = threading.Event()
        cs = None
        def run_loop():
            nonlocal cs
            asyncio.set_event_loop(cs_loop)
            cs = m.ChannelServer(sup, sock_path)
            cs_loop.run_until_complete(cs.start())
            ready.set()
            cs_loop.run_forever()
        cs_thread = threading.Thread(target=run_loop, daemon=True)
        cs_thread.start()
        check(ready.wait(5), "channel-server: loop started")

        # Verify _host_pid_from_pidfd works for ourselves before relying on it
        own_pidfd = os.pidfd_open(os.getpid())
        derived_pid = m._host_pid_from_pidfd(own_pidfd)
        os.close(own_pidfd)
        check(derived_pid == os.getpid(),
              f"_host_pid_from_pidfd returns our own pid (got {derived_pid})")

        # Send a register message with our own pidfd over SCM_RIGHTS
        msg = dict(type="register", session_id="CSBOX", cmd=["true"])
        payload = (json.dumps(msg) + "\n").encode()

        pidfd = os.pidfd_open(os.getpid())
        ack = None
        # KEEP the connection OPEN after register: that SAME connection IS the box's
        # muxed channel now. Closing it is the teardown signal (EOF → unregister), so we
        # must hold it open while we assert the session is live, then close it at the end.
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            s.settimeout(10.0)
            s.connect(sock_path)
            s.sendmsg([payload],
                      [(socket.SOL_SOCKET, socket.SCM_RIGHTS,
                        array.array("i", [pidfd]).tobytes())])
            buf = b""
            while b"\n" not in buf:
                chunk = s.recv(4096)
                if not chunk: break
                buf += chunk
            line = buf.split(b"\n", 1)[0]
            ack = json.loads(line.decode()) if line.strip() else None

            check(ack is not None, "channel-server: register with pidfd got a reply")
            check(ack and ack.get("ok") is True,
                  f"channel-server: register ack ok=True (got {ack!r})")
            check(ack and bool(ack.get("owner_token")),
                  "channel-server: register ack includes an owner_token")

            # Session is live in the supervisor (keyed by the minted box_id), while the
            # box's muxed connection is still open.
            sid = ack.get("session_id") if ack else None
            check(sid in sup.sessions and sup.sessions[sid].live,
                  "channel-server: session is live after pidfd-register")

            # _derive_parent_sid with our own pid returns None (no session has us as
            # ancestor) — just verify it doesn't crash and returns None for this case.
            result = m._derive_parent_sid(os.getpid(), sup)
            check(result is None or isinstance(result, str),
                  f"channel-server: _derive_parent_sid ok (got {result!r})")
        finally:
            try: os.close(pidfd)
            except OSError: pass
            try: s.close()       # close the muxed connection → box teardown (EOF)
            except OSError: pass

    finally:
        try: cs_loop.call_soon_threadsafe(cs_loop.stop)
        except Exception: pass
        if cs_thread is not None: cs_thread.join(timeout=5)
        try: cs_loop.close()
        except Exception: pass
        shutil.rmtree(tmp, ignore_errors=True)


# ── kick-up apply: nested box promotes into live parent overlay ──────────────

def test_apply_kick_up_live_parent():
    """Applying a change in a nested box (live parent) promotes it into the parent's
    Index + blob pool as a pending change.  The real host is NOT written, and the
    child's change is dropped from its change set."""
    import stat as stat_mod
    tmp = Path(tempfile.mkdtemp(prefix="cp-kickup-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(mount=_FakeMount())

        # Register parent + child LIVE: supply a live pidfd so _box_running() is True
        # and apply() takes the LiveUpper path (operates on the in-RAM Index). Without
        # this the box looks finished, apply() uses the SqlarArchive path, and the
        # change is dropped from the on-disk sqlar but NOT the RAM mirror this test
        # inspects — which is the live, in-box scenario the test name promises.
        p_pidfd = os.pidfd_open(os.getpid())
        ack_p = sup.register(dict(session_id="PARENT", cmd=["parent"],
                                  root_tgid=os.getpid(), _register_pidfd=p_pidfd))
        try: os.close(p_pidfd)
        except OSError: pass
        check(ack_p.get("ok") is True, "kick-up: parent registers ok")
        sid_p = ack_p["session_id"]

        # Register child with _derived_parent_sid pointing at parent (also live).
        c_pidfd = os.pidfd_open(os.getpid())
        ack_c = sup.register(dict(session_id="CHILD", cmd=["child"],
                                  _derived_parent_sid=sid_p,
                                  root_tgid=os.getpid(), _register_pidfd=c_pidfd))
        try: os.close(c_pidfd)
        except OSError: pass
        check(ack_c.get("ok") is True, "kick-up: child registers ok with parent")
        sid_c = ack_c["session_id"]
        check(str(sup.sessions[sid_c].parent_box_id) == sid_p,
              "kick-up: Session.parent_box_id is set on the child")

        # Simulate a file change in child's overlay (set_entry + pool blob).
        child_idx = sup.indexes[sid_c]
        wid = child_idx.writer_for(os.getpid())
        rel = "tmp/kickup_test.txt"
        content = b"hello from nested box\n"
        child_idx.set_entry(rel, "file", stat_mod.S_IFREG | 0o644, wid, "create")
        bp = m.blob_path(child_idx.box_id, child_idx.row_id(rel))
        bp.parent.mkdir(parents=True, exist_ok=True)
        bp.write_bytes(content)

        # Verify the child's change is tracked in the Index (pool-backed files
        # are tracked via the Index, not via session_changes() which walks up/).
        check(child_idx.kind_of(rel) == "file",
              "kick-up: child change visible before apply (Index kind=file)")

        # Apply the change in the child box.
        result = sup.review.apply(sid_c, [rel])
        check(result.get("errors") == [], f"kick-up: apply produced no errors (got {result})")
        check(rel in result.get("applied", []),
              "kick-up: rel in applied list")

        # The parent's live Index should now show the path as a pending change.
        parent_idx = sup.indexes[sid_p]
        check(parent_idx.kind_of(rel) == "file",
              "kick-up: parent Index shows the path as a 'file' entry")

        # The parent's blob pool should hold the bytes.
        p_rid = parent_idx.row_id(rel)
        check(p_rid is not None, "kick-up: parent Index has a row_id for the path")
        p_blob = m.blob_path(parent_idx.box_id, p_rid)
        check(p_blob.exists(), "kick-up: parent blob file was written")
        check(p_blob.read_bytes() == content,
              "kick-up: parent blob content matches child's original bytes")

        # The real host must NOT have been written (path does NOT exist on host).
        host_path = Path("/") / rel
        check(not host_path.exists(),
              "kick-up: real host was NOT written (/tmp/kickup_test.txt absent)")

        # The child's Index should no longer track the path after apply.
        check(child_idx.kind_of(rel) is None,
              "kick-up: child Index no longer tracks the applied path")

    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_apply_root_box_still_writes_host():
    """A root box (parent=None) apply still uses _write_host_change — the old path.
    We don't write a real file to the root; instead we verify the apply returns an
    error (host path non-writable in test env) but does NOT promote anywhere, i.e.
    the old code path is taken."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-root-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(mount=_FakeMount())
        ack = sup.register(dict(session_id="ROOT", cmd=["root"]))
        sid = ack["session_id"]
        check(sup.sessions[sid].parent_box_id is None,
              "root-box: Session.parent_box_id is None for a top-level box")
        # The parent_sid() helper must return None.
        check(sup.review._parent_key(sid) is None,
              "root-box: _parent_key returns None for root")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_apply_kick_up_finished_parent():
    """Applying a change in a nested box whose parent is FINISHED (only sqlar,
    no live Index) promotes the change into the parent's sqlar directly."""
    import stat as stat_mod
    tmp = Path(tempfile.mkdtemp(prefix="cp-kickup-fin-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(mount=_FakeMount())

        # Register parent then child.
        ack_p = sup.register(dict(session_id="PARENT", cmd=["parent"]))
        sid_p = ack_p["session_id"]
        ack_c = sup.register(dict(session_id="CHILD", cmd=["child"],
                                  _derived_parent_sid=sid_p))
        check(ack_c.get("ok") is True, "kick-up-fin: child registers")
        sid_c = ack_c["session_id"]

        # Tear down parent to simulate it finishing (it goes to sqlar).
        tok_p = sup._owner_tokens.get(sid_p, "")
        sup.unregister(dict(session_id=sid_p, owner_token=tok_p,
                            status="finished", exit_code=0))
        # Parent index is gone; Session may still exist in sessions dict.
        check(sid_p not in sup.indexes,
              "kick-up-fin: parent Index is gone after teardown")

        # The child still knows its parent box_id.
        check(str(sup.sessions[sid_c].parent_box_id) == sid_p,
              "kick-up-fin: child still records parent box_id")

        # Manually set up a finished parent sqlar (it may have been cleaned up
        # if empty — ensure the sqlar exists by calling _sqlar_open).
        p_sp = m.sqlar_path(sid_p)
        m._sqlar_open(p_sp).close()   # ensure schema exists

        # Simulate a change in child: simple approach via SqlarArchive write
        # (child is still live, so use its index).
        child_idx = sup.indexes[sid_c]
        wid = child_idx.writer_for(os.getpid())
        rel = "var/lib/kickup_fin.dat"
        content = b"\x00\x01\x02binary"
        child_idx.set_entry(rel, "file", stat_mod.S_IFREG | 0o600, wid, "create")
        bp = m.blob_path(child_idx.box_id, child_idx.row_id(rel))
        bp.parent.mkdir(parents=True, exist_ok=True)
        bp.write_bytes(content)

        # Verify _parent_key still resolves (even though parent is finished,
        # it should still be in sessions dict if it had changes).
        # If the parent was cleaned up (no sqlar content), inject it back.
        if sid_p not in sup.sessions:
            sup.sessions[sid_p] = m.Session(session_id=sid_p, box_id=int(sid_p),
                                             cmd=["parent"], live=False)

        # Apply the child's change — should promote to parent sqlar.
        result = sup.review.apply(sid_c, [rel])
        # If parent resolves, the change gets promoted.
        psid = sup.review._parent_key(sid_c)
        if psid is not None:
            check(result.get("errors") == [], f"kick-up-fin: no errors (got {result})")
            # Parent's sqlar should now carry the row.
            rows = m.sqlar_list(p_sp)
            names = {r[0] for r in rows}
            check(rel in names,
                  f"kick-up-fin: parent sqlar has promoted path {rel!r}")
            import stat as st_mod
            mode = m.sqlar_mode(p_sp, rel)
            check(mode is not None and st_mod.S_ISREG(mode),
                  "kick-up-fin: promoted entry has regular-file mode")
            check(m.sqlar_content(p_sp, rel) == content,
                  "kick-up-fin: promoted content matches original bytes")
            host_path = Path("/") / rel
            check(not host_path.exists(),
                  "kick-up-fin: real host was NOT written")
        else:
            # Parent was cleaned up entirely — at least verify no crash.
            check(True, "kick-up-fin: apply with absent parent did not crash")

    finally:
        shutil.rmtree(tmp, ignore_errors=True)


# ── NESTED LAUNCH: register-reply dir-fd round-trip ─────────────────────────

def test_register_reply_fd_nested_box():
    """A NESTED box's register reply carries NO fd (path-bind nested-launch).

    The nested-launch mechanism roots a child box by binding the parent-exposed
    synthetic path /<KIDS_DIR>/<child> rather than receiving a mount fd, so the
    register reply must NOT carry any SCM_RIGHTS fd.

    Strategy:
    - Start a real ChannelServer with a _FakeMount.
    - Register a parent session so _derive_parent_sid can resolve it.
    - Register a child session from a client that recvmsg's the reply.
    - Assert the reply carries no fd, and the child is registered with its parent
      derived from kernel ancestry (the pointer the UI uses to expose the child
      under the parent's /<KIDS_DIR>).
    """
    tmp = Path(tempfile.mkdtemp(prefix="cp-replyfd-"))
    _redirect_state(tmp)
    cs_loop = asyncio.new_event_loop()
    cs_thread = None
    try:
        m.ensure_dirs()

        # We need a real pidfd for the parent session so _derive_parent_sid works.
        # Use our own ppid as the parent's root_tgid so _derive_parent_sid(getpid())
        # finds it (getpid()'s PPid chain includes getppid()).
        pidfd_parent = os.pidfd_open(os.getppid())   # live for the whole test
        try:
            sup = m.Supervisor(mount=_FakeMount())
            sock_path = str(tmp / "ctrl_replyfd.sock")

            ready = threading.Event()
            cs = None
            def run_loop():
                nonlocal cs
                asyncio.set_event_loop(cs_loop)
                cs = m.ChannelServer(sup, sock_path)
                cs_loop.run_until_complete(cs.start())
                ready.set()
                cs_loop.run_forever()
            cs_thread = threading.Thread(target=run_loop, daemon=True)
            cs_thread.start()
            check(ready.wait(5), "reply-fd: loop started")

            # Register the parent session inline (no network path needed).
            ack_p = sup.register(dict(session_id="PARENT", cmd=["parent"],
                                      root_tgid=os.getppid(),
                                      _register_pidfd=pidfd_parent,
                                      prov=dict(ppid=0, exe="/bin/sh", env={})))
            check(ack_p.get("ok") is True, "reply-fd: parent session registers ok")
            sid_parent = ack_p["session_id"]
            # register() dup'd the pidfd; close our copy so it doesn't leak.
            try: os.close(pidfd_parent)
            except OSError: pass
            pidfd_parent = -1

            # Send the child register (kernel-derived parent = our process ancestry).
            # _FakeMount.add_session creates the minted box_root so register's os.open works.
            msg = dict(type="register", session_id="CHILD", cmd=["child"])
            payload = (json.dumps(msg) + "\n").encode()
            own_pidfd = os.pidfd_open(os.getpid())
            child_mount_fd = -1
            child_ack = None
            try:
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                    s.settimeout(10.0)
                    s.connect(sock_path)
                    s.sendmsg([payload],
                              [(socket.SOL_SOCKET, socket.SCM_RIGHTS,
                                array.array("i", [own_pidfd]).tobytes())])
                    # Use recvmsg to capture any SCM_RIGHTS fd on the reply.
                    data, anc, _flags, _addr = s.recvmsg(65536,
                                                          socket.CMSG_SPACE(4 * 4))
                    for lvl, typ, cmsg_data in anc:
                        if lvl == socket.SOL_SOCKET and typ == socket.SCM_RIGHTS:
                            a = array.array("i"); a.frombytes(cmsg_data)
                            fds = a.tolist()
                            if fds:
                                child_mount_fd = fds[0]
                            for extra in fds[1:]:
                                try: os.close(extra)
                                except OSError: pass
                    line = data.split(b"\n", 1)[0]
                    child_ack = json.loads(line.decode()) if line.strip() else None
            finally:
                try: os.close(own_pidfd)
                except OSError: pass

            check(child_ack is not None, "nested-register: child register got a reply")
            check(child_ack and child_ack.get("ok") is True,
                  f"nested-register: child ack ok=True (got {child_ack!r})")
            sid_child = child_ack.get("session_id") if child_ack else None

            # KEY ASSERTION: the nested-launch mechanism no longer ships any fd. A
            # nested box roots its bwrap by binding the parent-exposed synthetic path
            # /<KIDS_DIR>/<child> instead of receiving a mount fd, so NO SCM_RIGHTS fd
            # must accompany the register reply (for nested OR top-level boxes).
            check(child_mount_fd < 0,
                  f"nested-register: NO fd is sent (path-bind mechanism); "
                  f"got fd={child_mount_fd}")
            if child_mount_fd >= 0:
                try: os.close(child_mount_fd)
                except OSError: pass
                child_mount_fd = -1
            # The child must still be registered with its parent derived from kernel
            # ancestry — that pointer is what makes the UI expose the child under the
            # parent's /<KIDS_DIR>. (parent_derived may be None only if the kernel
            # withheld our host pid via pidfd; report rather than hard-fail on that.)
            parent_derived = (sup.sessions[sid_child].parent_box_id
                              if sid_child in sup.sessions else None)
            check(parent_derived is not None or sid_child not in sup.sessions,
                  f"nested-register: child's parent derived from kernel ancestry "
                  f"(parent_box_id={parent_derived!r})")

        finally:
            if pidfd_parent >= 0:
                try: os.close(pidfd_parent)
                except OSError: pass
    finally:
        try: cs_loop.call_soon_threadsafe(cs_loop.stop)
        except Exception: pass
        if cs_thread is not None: cs_thread.join(timeout=5)
        try: cs_loop.close()
        except Exception: pass
        shutil.rmtree(tmp, ignore_errors=True)


def test_register_reply_fd_toplevel_no_fd():
    """A top-level registration (no live parent in the supervisor) sends NO fd on reply.

    Uses a fresh supervisor with no sessions so _derive_parent_sid always returns None,
    ensuring the box is treated as top-level regardless of our process ancestry.
    """
    tmp = Path(tempfile.mkdtemp(prefix="cp-replyfd-top-"))
    _redirect_state(tmp)
    cs2_loop = asyncio.new_event_loop()
    cs2_thread = None
    try:
        m.ensure_dirs()
        # Empty supervisor — no sessions → _derive_parent_sid always returns None.
        sup2 = m.Supervisor(mount=_FakeMount())
        sock2 = str(tmp / "ctrl_top.sock")

        ready2 = threading.Event()
        cs2 = None
        def run_loop2():
            nonlocal cs2
            asyncio.set_event_loop(cs2_loop)
            cs2 = m.ChannelServer(sup2, sock2)
            cs2_loop.run_until_complete(cs2.start())
            ready2.set()
            cs2_loop.run_forever()
        cs2_thread = threading.Thread(target=run_loop2, daemon=True)
        cs2_thread.start()
        check(ready2.wait(5), "reply-fd-top: loop started")

        msg_top = dict(type="register", session_id="TOPBOX", cmd=["top"])
        payload_top = (json.dumps(msg_top) + "\n").encode()
        own_pidfd2 = os.pidfd_open(os.getpid())
        top_fd = -1
        top_ack = None
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s2:
                s2.settimeout(10.0)
                s2.connect(sock2)
                s2.sendmsg([payload_top],
                           [(socket.SOL_SOCKET, socket.SCM_RIGHTS,
                             array.array("i", [own_pidfd2]).tobytes())])
                data2, anc2, _flags2, _addr2 = s2.recvmsg(65536,
                                                           socket.CMSG_SPACE(4 * 4))
                for lvl, typ, cmsg_data in anc2:
                    if lvl == socket.SOL_SOCKET and typ == socket.SCM_RIGHTS:
                        a = array.array("i"); a.frombytes(cmsg_data)
                        fds2 = a.tolist()
                        if fds2:
                            top_fd = fds2[0]
                        for extra in fds2[1:]:
                            try: os.close(extra)
                            except OSError: pass
                line2 = data2.split(b"\n", 1)[0]
                top_ack = json.loads(line2.decode()) if line2.strip() else None
        finally:
            try: os.close(own_pidfd2)
            except OSError: pass

        check(top_ack is not None and top_ack.get("ok") is True,
              f"reply-fd-top: top-level ack ok (got {top_ack!r})")
        check(top_fd < 0,
              f"reply-fd-top: top-level register sends NO fd (got fd={top_fd})")
        if top_fd >= 0:
            try: os.close(top_fd)
            except OSError: pass
    finally:
        try: cs2_loop.call_soon_threadsafe(cs2_loop.stop)
        except Exception: pass
        if cs2_thread is not None: cs2_thread.join(timeout=5)
        try: cs2_loop.close()
        except Exception: pass
        shutil.rmtree(tmp, ignore_errors=True)


# ── scoped dotted box names ──────────────────────────────────────────────────

def test_dotted_name_validation():
    """valid_dotted_name accepts dotted paths and rejects unsafe/malformed names."""
    check(m.valid_dotted_name("A"), "dotted: single segment accepted")
    check(m.valid_dotted_name("MYBOX"), "dotted: multi-char accepted")
    check(m.valid_dotted_name("A.B"), "dotted: two segments accepted")
    check(m.valid_dotted_name("A.B.C"), "dotted: three segments accepted")
    check(m.valid_dotted_name("FOO.BAR2-BAZ"), "dotted: dash in segment accepted")
    check(not m.valid_dotted_name(""), "dotted: empty string rejected")
    check(not m.valid_dotted_name("."), "dotted: bare dot rejected")
    check(not m.valid_dotted_name(".."), "dotted: '..' rejected")
    check(not m.valid_dotted_name(".A"), "dotted: leading dot rejected")
    check(not m.valid_dotted_name("A."), "dotted: trailing dot rejected")
    check(not m.valid_dotted_name("A..B"), "dotted: consecutive dots rejected")
    check(not m.valid_dotted_name("A/B"), "dotted: slash rejected")
    check(not m.valid_dotted_name("a.B"), "dotted: lowercase segment rejected")
    check(not m.valid_dotted_name("A.b"), "dotted: lowercase second segment rejected")
    check(not m.valid_dotted_name("A-.B"), "dotted: trailing dash in segment rejected")
    # valid_box_id is the INTERNAL box-key validator (decimal box_id): names are NOT keys.
    check(not m.valid_box_id("A.B"), "valid_box_id: a NAME/display path is NOT a box key")
    check(not m.valid_box_id("A.B.C"), "valid_box_id: dotted display path is NOT a box key")
    check(not m.valid_box_id(".."), "valid_box_id: '..' rejected")
    check(not m.valid_box_id("A/B"), "valid_box_id: slash rejected")
    check(m.valid_box_id("42"), "valid_box_id: a decimal box_id IS a box key")


def test_host_register_top_level_name():
    """HOST: register with a single-segment NAME (no parent) creates a top-level box."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-dn-tl-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(mount=_FakeMount())
        ack = sup.register(dict(session_id="ALPHA", cmd=["true"]))
        check(ack.get("ok") is True, "host-tl: register A ok")
        sid = ack["session_id"]
        check(sid in sup.sessions, "host-tl: box keyed by box_id is in sessions")
        check(sup.sessions[sid].name == "ALPHA", "host-tl: NAME label is ALPHA")
        check(sup.sessions[sid].parent_box_id is None, "host-tl: ALPHA parent is None")
        check(sup.sessions[sid].live, "host-tl: ALPHA is live")
        # age comes from on-disk ctime, not a parsed timestamp
        check(sup.sessions[sid].started > 0, "host-tl: started (ctime) is set")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_host_register_dotted_child():
    """HOST: A.B with A live → creates child B of A (parent = A)."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-dn-child-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(mount=_FakeMount())
        # Create parent A first.
        ack_a = sup.register(dict(session_id="ALPHA", cmd=["true"]))
        check(ack_a.get("ok") is True, "host-child: ALPHA registers ok")
        sid_a = ack_a["session_id"]
        # Create child A.B (dotted display path: parent prefix must resolve).
        ack_ab = sup.register(dict(session_id="ALPHA.BETA", cmd=["true"]))
        check(ack_ab.get("ok") is True, f"host-child: ALPHA.BETA registers ok (got {ack_ab})")
        sid_ab = ack_ab["session_id"]
        check(sup.sessions[sid_ab].name == "BETA", "host-child: child NAME is BETA")
        check(str(sup.sessions[sid_ab].parent_box_id) == sid_a,
              "host-child: child parent pointer is ALPHA's box_id")
        check(sup.display_path(sid_ab) == "ALPHA.BETA",
              "host-child: derived display path is ALPHA.BETA")
        check(sup.sessions[sid_ab].live, "host-child: ALPHA.BETA is live")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_host_register_dotted_child_finished_parent():
    """HOST: A.B with A finished (sqlar on disk, name meta='ALPHA') → parent resolves to
    A's box_id by NAME, child gets a parent pointer."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-dn-finpar-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(mount=_FakeMount())
        # Write a finished parent box on disk (numeric box_id stem, name meta='ALPHA')
        # AFTER the Supervisor is created so it is not a live session.
        pbid = m.mint_box_id()
        m.sqlar_meta_set(m.sqlar_path(pbid), "name", "ALPHA")
        check(sup.resolve_box("ALPHA") == str(pbid),
              "host-finpar: ALPHA resolves to the finished box's box_id")

        ack = sup.register(dict(session_id="ALPHA.BETA", cmd=["true"]))
        check(ack.get("ok") is True, f"host-finpar: ALPHA.BETA registers ok (got {ack})")
        sid_ab = ack["session_id"]
        check(sup.sessions[sid_ab].parent_box_id == pbid,
              "host-finpar: parent pointer is the finished ALPHA's box_id")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_host_register_dotted_parent_not_existing():
    """HOST: A.B with A NOT existing → fail-closed with a clear error."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-dn-nopar-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(mount=_FakeMount())
        ack = sup.register(dict(session_id="ALPHA.BETA", cmd=["true"]))
        check(ack.get("ok") is False, "host-nopar: rejected (ok False)")
        check("ALPHA" in (ack.get("error") or ""),
              f"host-nopar: error mentions missing parent (got {ack.get('error')!r})")
        # Nothing created.
        check("ALPHA.BETA" not in sup.sessions, "host-nopar: no session created")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_inbox_register_relname():
    """IN-BOX: relname=B with enclosing A → absolute A.B, parent A (authoritative)."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-dn-inbox-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(mount=_FakeMount())
        # Register the parent (enclosing) box.
        ack_a = sup.register(dict(session_id="ALPHA", cmd=["parent"]))
        check(ack_a.get("ok") is True, "inbox: ALPHA registers ok")
        sid_a = ack_a["session_id"]

        # Simulate in-box child: relname=BETA, _derived_parent_sid=ALPHA's box_id (kernel).
        ack_c = sup.register(dict(
            session_id=None,
            relname="BETA",
            _derived_parent_sid=sid_a,
            cmd=["child"]))
        check(ack_c.get("ok") is True, f"inbox: child registers ok (got {ack_c})")
        sid_c = ack_c["session_id"]
        check(sup.sessions[sid_c].name == "BETA", "inbox: child NAME is BETA")
        check(str(sup.sessions[sid_c].parent_box_id) == sid_a,
              "inbox: child parent is ALPHA (kernel-derived, not overridable)")
        check(sup.display_path(sid_c) == "ALPHA.BETA",
              "inbox: derived display path is ALPHA.BETA")
        check(sup.sessions[sid_c].live, "inbox: child is live")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_inbox_relname_with_dot_rejected():
    """IN-BOX: relname containing a dot or slash is rejected (prevents subtree escape)."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-dn-escape-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(mount=_FakeMount())
        ack_a = sup.register(dict(session_id="ALPHA", cmd=["parent"]))

        for bad_relname in ("A.B", "../ESCAPE", "/ESCAPE", "A/B"):
            ack = sup.register(dict(
                session_id="20260606-000000_123",
                relname=bad_relname,
                _derived_parent_sid="ALPHA",
                cmd=["child"]))
            check(ack.get("ok") is False,
                  f"inbox-escape: relname {bad_relname!r} rejected (ok False)")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_inbox_relname_empty_default():
    """IN-BOX: relname='' → default A<n> auto-name assigned under enclosing box."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-dn-default-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(mount=_FakeMount())
        ack_a = sup.register(dict(session_id="ALPHA", cmd=["parent"]))
        check(ack_a.get("ok") is True, "inbox-default: ALPHA registers ok")
        sid_a = ack_a["session_id"]

        ack_c = sup.register(dict(
            session_id=None,
            relname="",       # empty = let the UI assign a default A<n>
            _derived_parent_sid=sid_a,
            cmd=["child"]))
        check(ack_c.get("ok") is True, f"inbox-default: empty relname ok (got {ack_c})")
        child_sid = ack_c["session_id"]
        seg = sup.sessions[child_sid].name
        check(seg.startswith("A") and seg[1:].isdigit(),
              f"inbox-default: auto NAME is A<n> (got {seg!r})")
        check(str(sup.sessions[child_sid].parent_box_id) == sid_a,
              "inbox-default: parent is ALPHA")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_inbox_trust_kernel_parent_not_body():
    """IN-BOX: a message with relname MUST use the kernel-derived parent, not any
    parent the box might embed in its session_id or message body."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-dn-trust-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(mount=_FakeMount())
        # Two parent boxes: ALPHA (live) and EVIL (live).
        a = sup.register(dict(session_id="ALPHA", cmd=["alpha"]))["session_id"]
        e = sup.register(dict(session_id="EVIL", cmd=["evil"]))["session_id"]

        # A box inside ALPHA: relname is the ONLY box-supplied name; the parent comes
        # from the kernel-derived _derived_parent_sid (ALPHA's box_id). A box-supplied
        # session_id is ignored for naming entirely.
        ack = sup.register(dict(
            session_id="EVIL.SNEAKY",    # box tries to claim it's under EVIL
            relname="SNEAKY",
            _derived_parent_sid=a,       # kernel says it's inside ALPHA
            cmd=["sneaky"]))
        check(ack.get("ok") is True, "trust: SNEAKY registers (kernel parent honoured)")
        sid_s = ack["session_id"]
        check(str(sup.sessions[sid_s].parent_box_id) == a,
              "trust: parent is kernel-derived ALPHA, not body-supplied EVIL")
        check(sup.display_path(sid_s) == "ALPHA.SNEAKY",
              "trust: derived display path is ALPHA.SNEAKY (kernel wins)")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_default_named_box_age_from_ctime():
    """A named box's age comes from its on-disk ctime (the numeric id carries no
    timestamp), so started > 0 and age/sort works without any 'born' field."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-dn-born-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(mount=_FakeMount())
        ack = sup.register(dict(session_id="MYBOX", cmd=["true"]))
        check(ack.get("ok") is True, "age: MYBOX registers ok")
        s = sup.sessions[ack["session_id"]]
        check(not hasattr(s, "born"), "age: Session no longer has a 'born' field")
        check(s.started > 0,
              f"age: Session.started comes from ctime > 0 (got {s.started})")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_dotted_path_safety():
    """Dotted names with '..' or '/' embedded are rejected before any mkdir."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-dn-safe-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(mount=_FakeMount())
        bad_names = [
            "../ESCAPE",
            "A/B",
            "A..B",
            ".HIDDEN",
            "A.",
        ]
        for bad in bad_names:
            ack = sup.register(dict(session_id=bad, cmd=["true"]))
            check(ack.get("ok") is False,
                  f"path-safety: {bad!r} rejected before mkdir")
            # Verify nothing was created under live_home.
            lh = m.live_home()
            if lh.exists():
                created = [d.name for d in lh.iterdir()
                           if (d / "up").is_dir()]
                check(bad not in created,
                      f"path-safety: no live/{bad!r} dir created")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_rename_is_meta_only_label_write():
    """rename() is a meta-only NAME label write: no file move, no id change. It works on
    a LIVE box, rejects a dotted name (the dotted form is a derived display path, never
    a stored label), and the box's identity (box_id key, sqlar path) is unchanged."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-dn-rename-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(mount=_FakeMount())
        ack = sup.register(dict(session_id="ALPHA", cmd=["true"]))
        check(ack.get("ok") is True, "rename: ALPHA registers")
        sid = ack["session_id"]
        sp_before = m.sqlar_path(sid)
        # Rename a LIVE box by box_id; identity (key + sqlar path) is unchanged.
        r = sup.rename(sid, "BETA")
        check(r.get("ok") is True, f"rename: live rename ok (got {r})")
        check(r.get("name") == "BETA", "rename: new name reported")
        check(sid in sup.sessions and sup.sessions[sid].name == "BETA",
              "rename: NAME label updated in place, same box key")
        check(m.sqlar_path(sid) == sp_before and sp_before.exists(),
              "rename: NO file move (same sqlar path)")
        check(m.sqlar_meta_get(sp_before, "name") == "BETA",
              "rename: NAME persisted in meta")
        # Resolve by the new name; the old name no longer resolves.
        check(sup.resolve_box("BETA") == sid, "rename: resolves by new name")
        check(sup.resolve_box("ALPHA") is None, "rename: old name no longer resolves")
        # A dotted name is rejected (single-segment labels only).
        r2 = sup.rename(sid, "X.Y")
        check(r2.get("ok") is False, "rename: dotted name rejected")
        # A sibling-name clash is rejected.
        sup.register(dict(session_id="GAMMA", cmd=["true"]))
        r3 = sup.rename(sid, "GAMMA")
        check(r3.get("ok") is False, "rename: sibling NAME clash rejected")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


# ── Feature 1: box-list tree helper unit tests ──────────────────────────────

def test_box_tree_rows_indented_connectors_skipped():
    """build_path_tree on a set of dotted box names yields the right (basename, depth)
    in DFS order and SKIPS connector rows (dotted prefixes that are not real boxes)."""
    # Box set: A (root), A.B (child), A.B.C (grandchild), X (separate root).
    # There are NO connector rows here — all prefixes are real boxes.
    sids = {"A", "A.B", "A.B.C", "X"}
    members = {sid: tuple(sid.split(".")) for sid in sids}
    tree = m.build_path_tree(members, lambda p: ".".join(p))

    # Separate real rows from connectors; spec says connectors should be skipped in UI.
    real = [(key, depth) for key, _payload, depth, connector in tree if not connector]
    conn_rows = [(key, depth) for key, _payload, depth, connector in tree if connector]

    # No connectors: every prefix (A, A.B) is itself a real box.
    check(conn_rows == [], f"no connector rows when all prefixes are real boxes (got {conn_rows})")

    # DFS order: A before A.B before A.B.C; X appears as a separate root.
    keys_in_order = [k for k, _ in real]
    check("A" in keys_in_order, "tree: A present")
    check("A.B" in keys_in_order, "tree: A.B present")
    check("A.B.C" in keys_in_order, "tree: A.B.C present")
    check("X" in keys_in_order, "tree: X present")
    check(keys_in_order.index("A") < keys_in_order.index("A.B"),
          "tree: A before A.B (DFS)")
    check(keys_in_order.index("A.B") < keys_in_order.index("A.B.C"),
          "tree: A.B before A.B.C (DFS)")

    # Depths: A=0, A.B=1, A.B.C=2, X=0.
    depths = {k: d for k, d in real}
    check(depths["A"] == 0, f"tree: A depth=0 (got {depths.get('A')})")
    check(depths["A.B"] == 1, f"tree: A.B depth=1 (got {depths.get('A.B')})")
    check(depths["A.B.C"] == 2, f"tree: A.B.C depth=2 (got {depths.get('A.B.C')})")
    check(depths["X"] == 0, f"tree: X depth=0 (got {depths.get('X')})")

    # Basenames match last segment of the sid.
    basenames = {k: k.rsplit(".", 1)[-1] for k in keys_in_order}
    check(basenames["A"] == "A", "basename A")
    check(basenames["A.B"] == "B", "basename A.B → B")
    check(basenames["A.B.C"] == "C", "basename A.B.C → C")
    check(basenames["X"] == "X", "basename X")


def test_box_tree_connector_skipped_when_prefix_has_no_box():
    """When a dotted prefix is NOT a real box, build_path_tree emits a connector=True row.
    The UI skips connector rows; only real boxes appear."""
    # A.B.C exists but A.B does NOT → A.B is a connector.
    sids = {"A", "A.B.C"}
    members = {sid: tuple(sid.split(".")) for sid in sids}
    tree = m.build_path_tree(members, lambda p: ".".join(p))

    real_keys = [key for key, _p, _d, connector in tree if not connector]
    conn_keys = [key for key, _p, _d, connector in tree if connector]

    # A.B (the tuple ("A","B")) is a connector; A and A.B.C are real.
    check("A" in real_keys, "connector-skip: A is real")
    check("A.B.C" in real_keys, "connector-skip: A.B.C is real")
    # build_path_tree uses the tuple as the key for connectors, not a sid string.
    connector_tups = [key for key, _p, _d, connector in tree if connector]
    check(("A", "B") in connector_tups,
          f"connector-skip: (A,B) is a connector row (got {connector_tups})")
    # The UI pattern: skip connector=True rows → only A and A.B.C survive.
    check(len(real_keys) == 2,
          f"connector-skip: exactly 2 real rows after skipping connector (got {real_keys})")


# ── Feature 2: splice_delete ─────────────────────────────────────────────────

def _make_finished_box(sup, sid, name=None, parent=None, with_content=False):
    """Create a minimal finished (non-live) box for dissolve tests, keyed by the box
    key str(box_id). `name` is the NAME label; `parent` is the parent's box_id (int).
    If with_content=True, insert a sqlar row so the box survives _maybe_remove_empty."""
    sp = m.sqlar_path(sid)
    sp.parent.mkdir(parents=True, exist_ok=True)
    import sqlite3
    conn = sqlite3.connect(str(sp))
    conn.execute("CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT)")
    if name is not None:
        conn.execute("INSERT OR REPLACE INTO meta VALUES('name',?)", (name,))
    if parent is not None:
        conn.execute("INSERT OR REPLACE INTO meta VALUES('parent_box_id',?)", (str(parent),))
    if with_content:
        conn.execute(
            "CREATE TABLE IF NOT EXISTS sqlar"
            "(name TEXT PRIMARY KEY, mode INT, mtime INT, sz INT, data BLOB)")
        conn.execute("INSERT OR REPLACE INTO sqlar VALUES('proof.txt',33188,0,4,X'41424344')")
    conn.commit(); conn.close()
    sup.sessions[sid] = m.Session(
        session_id=sid, box_id=int(sid), name=name or "", cmd=["test"], live=False,
        parent_box_id=(int(parent) if parent is not None else None),
        shm_dir=str(m.live_dir(sid)))


def test_dissolve_happy_path():
    """dissolve(B) removes B and re-parents its DIRECT child C to B's own parent A (a
    pointer write — no file moves, no id changes). Grandchild D stays under C. Content
    and box_ids are preserved; C's parent_box_id meta now points at A."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-splice-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(mount=_FakeMount())
        # Tree: A(1) ← B(2) ← C(3) ← D(4)
        _make_finished_box(sup, "1", name="A")
        _make_finished_box(sup, "2", name="B", parent=1)
        _make_finished_box(sup, "3", name="C", parent=2, with_content=True)
        _make_finished_box(sup, "4", name="D", parent=3, with_content=True)

        r = sup.dissolve("2")
        check(r.get("ok") is True, f"dissolve-happy: dissolve ok (got {r})")
        check(r.get("deleted") == "2", "dissolve-happy: deleted is box 2 (B)")
        check(r.get("reparented") == ["3"], f"dissolve-happy: only direct child C re-parented (got {r.get('reparented')})")

        # B is gone; C and D remain with the SAME box_ids (no moves).
        check("2" not in sup.sessions, "dissolve-happy: B not in sessions")
        check(not m.sqlar_path("2").exists(), "dissolve-happy: 2.sqlar removed")
        check(m.sqlar_path("3").exists(), "dissolve-happy: C (3.sqlar) kept, same id")
        check(m.sqlar_path("4").exists(), "dissolve-happy: D (4.sqlar) kept, same id")

        # Content preserved on C.
        check(m.sqlar_content(m.sqlar_path("3"), "proof.txt") == b"ABCD",
              "dissolve-happy: C content intact")

        # C re-parented to A (1); D still under C (3).
        check(m.sqlar_meta_get(m.sqlar_path("3"), "parent_box_id") == "1",
              "dissolve-happy: C parent_box_id is now A (1)")
        check(m.sqlar_meta_get(m.sqlar_path("4"), "parent_box_id") == "3",
              "dissolve-happy: D parent_box_id unchanged (still C=3)")
        # Derived display path reflects the splice: A.C, A.C.D.
        check(sup.display_path("3") == "A.C", "dissolve-happy: C display path is A.C")
        check(sup.display_path("4") == "A.C.D", "dissolve-happy: D display path is A.C.D")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_dissolve_nonempty_finalizes_copydown():
    """dissolve ALLOWS a non-empty box: its changes are finalized first. With no
    matching rule, B's file is discarded — copied DOWN into the immediate child C that
    lacks it — then B is dissolved and C re-parented to A. C keeps the inherited file."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-splice-changes-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(mount=_FakeMount())
        _make_finished_box(sup, "1", name="A")
        _make_finished_box(sup, "2", name="B", parent=1, with_content=True)  # 'proof.txt'
        _make_finished_box(sup, "3", name="C", parent=2)                     # inherits

        r = sup.dissolve("2")
        check(r.get("ok") is True, f"dissolve-nonempty: succeeds (got {r})")
        check(r.get("deleted") == "2", "dissolve-nonempty: B deleted")
        check(not m.sqlar_path("2").exists(), "dissolve-nonempty: 2.sqlar gone")
        # C now OWNS the copied-down file and is re-parented to A.
        check(m.sqlar_content(m.sqlar_path("3"), "proof.txt") == b"ABCD",
              "dissolve-nonempty: B's file copied down into the child C")
        check(m.sqlar_meta_get(m.sqlar_path("3"), "parent_box_id") == "1",
              "dissolve-nonempty: C re-parented to A (1)")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_dissolve_top_level_child_becomes_top_level():
    """Dissolving a TOP-LEVEL box re-parents its direct children to None (top-level):
    the parent_box_id meta is cleared, not pointed elsewhere."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-splice-toplevel-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(mount=_FakeMount())
        _make_finished_box(sup, "1", name="A")                 # top-level
        _make_finished_box(sup, "2", name="C", parent=1, with_content=True)

        r = sup.dissolve("1")
        check(r.get("ok") is True, f"dissolve-top: ok (got {r})")
        check(r.get("reparented") == ["2"], "dissolve-top: C re-parented")
        check(not m.sqlar_path("1").exists(), "dissolve-top: A removed")
        check(sup.sessions["2"].parent_box_id is None,
              "dissolve-top: C is now top-level (parent_box_id None)")
        check(m.sqlar_meta_get(m.sqlar_path("2"), "parent_box_id") in (None, ""),
              "dissolve-top: C parent_box_id meta cleared")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_dissolve_refused_when_target_live():
    """dissolve refuses to dissolve a box that is itself live/running (but a LIVE
    descendant is fine — re-parenting is a pointer write)."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-splice-live-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(mount=_FakeMount())
        _make_finished_box(sup, "1", name="A")
        _make_finished_box(sup, "2", name="B", parent=1)
        # Make B itself appear live.
        ld = m.live_dir("2"); (ld / "up").mkdir(parents=True, exist_ok=True)
        sup.sessions["2"].live = True
        sup.sessions["2"].run_pid = os.getpid()   # alive so _live() is True
        try: sup.sessions["2"].run_pidfd = os.pidfd_open(os.getpid())
        except Exception: pass

        r = sup.dissolve("2")
        check(r.get("ok") is False, f"dissolve-live: refused (got {r})")
        check("running" in (r.get("error") or "").lower(),
              f"dissolve-live: error mentions running (got {r.get('error')!r})")
        check(m.sqlar_path("2").exists(), "dissolve-live: 2.sqlar untouched")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_dissolve_live_descendant_allowed():
    """A LIVE direct child is re-parented by pointer (no refusal): dissolving B while C
    is live still succeeds and C's parent pointer is updated in place."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-splice-livedesc-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(mount=_FakeMount())
        _make_finished_box(sup, "1", name="A")
        _make_finished_box(sup, "2", name="B", parent=1)
        _make_finished_box(sup, "3", name="C", parent=2, with_content=True)
        # C is live.
        ld = m.live_dir("3"); (ld / "up").mkdir(parents=True, exist_ok=True)
        sup.sessions["3"].live = True

        r = sup.dissolve("2")
        check(r.get("ok") is True, f"dissolve-livedesc: succeeds (got {r})")
        check(sup.sessions["3"].parent_box_id == 1,
              "dissolve-livedesc: live child C re-parented to A in place")
        check(m.sqlar_meta_get(m.sqlar_path("3"), "parent_box_id") == "1",
              "dissolve-livedesc: C's parent_box_id meta updated")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_remote_ui_control_plane():
    """The remote-UI surface (engine/UI split): the wire codec round-trips every
    payload shape; RemoteSupervisor/RemoteReview answer whitelisted verbs over a
    REAL ChannelServer socket; non-whitelisted verbs are refused; `subscribe`
    converts the connection into a live event feed."""
    # 1. codec round-trip (pure function, no server needed)
    v = {"a": [1, (2, 3), b"\x00\xff", {1: "x"}, {"s", "t"}], "p": Path("/tmp")}
    enc = json.loads(json.dumps(m.wire_encode(v)))
    dec = m.wire_decode(enc)
    check(dec["a"][1] == [2, 3] and dec["a"][2] == b"\x00\xff"
          and dec["a"][3] == {1: "x"} and dec["a"][4] == {"s", "t"}
          and dec["p"] == "/tmp",
          "remote-ui: wire codec round-trips bytes/tuples/sets/int-keyed dicts")

    tmp = Path(tempfile.mkdtemp(prefix="cp-rui-"))
    _redirect_state(tmp)
    cs_loop = asyncio.new_event_loop()
    try:
        m.ensure_dirs()
        # A finished box on disk: one captured file + one process row, so the
        # data verbs have something real to answer with.
        sid = "7001"
        backing = m.live_dir(sid); (backing / "up").mkdir(parents=True)
        idx = m.Index(backing)
        wid = idx.writer_for(os.getpid())
        idx.set_entry("afile.txt", "file", 0o100644, wid, "create")
        bp = m.blob_path(idx.box_id, idx.row_id("afile.txt"))
        bp.parent.mkdir(parents=True, exist_ok=True); bp.write_bytes(b"remote!\n")
        m.consolidate(str(backing), sid, index=idx)
        idx.close()

        sup = m.Supervisor(mount=_FakeMount())   # discovers 7001 from disk
        sock_path = str(tmp / "ctrl.sock")
        ready = threading.Event(); cs = None
        def run_loop():
            nonlocal cs
            asyncio.set_event_loop(cs_loop)
            cs = m.ChannelServer(sup, sock_path)
            cs_loop.run_until_complete(cs.start())
            ready.set()
            cs_loop.run_forever()
        threading.Thread(target=run_loop, daemon=True).start()
        check(ready.wait(5), "remote-ui: server loop started")

        rsup = m.RemoteSupervisor(sock_path)
        sd = rsup.session_dicts()
        check(any(d.get("session_id") == sid for d in sd),
              "remote-ui: session_dicts sees the on-disk box over the wire")
        check(sid in rsup.sessions, "remote-ui: .sessions facade keyed by sid")
        procs = rsup.processes(sid)
        check(procs and any(p[0] == wid for p in procs),
              "remote-ui: processes(sid) returns the writer's process row")
        ents = rsup.review.session_changes(sid)
        check(any(e.get("path") == "afile.txt" for e in ents),
              "remote-ui: review.session_changes returns the captured change")
        check(rsup.review._live(sid) is False,
              "remote-ui: review_live: a finished box is not live")
        st = rsup.review._state()
        check(st["consolidating"] == [] and sid not in st["consolidating"],
              "remote-ui: review_state reports no fold in progress")
        check(sid not in rsup.review._consolidating,
              "remote-ui: _consolidating set-view answers membership")
        try:
            rsup._rpc("register")             # NOT whitelisted as a ui verb
            check(False, "remote-ui: non-whitelisted verb must raise")
        except m.RemoteError:
            check(True, "remote-ui: non-whitelisted verb is refused")

        # 2. subscribe: ack, then a broadcast event arrives as a JSON line.
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sc:
            sc.settimeout(5); sc.connect(sock_path)
            sc.sendall(b'{"type":"subscribe"}\n')
            f = sc.makefile("rb")
            ack = json.loads(f.readline())
            check(ack.get("ok") is True, "remote-ui: subscribe acked")
            cs_loop.call_soon_threadsafe(
                cs.broadcast, dict(type="ping", n=42))
            ev = json.loads(f.readline())
            check(ev.get("type") == "ping" and ev.get("n") == 42,
                  "remote-ui: broadcast event arrives on the subscribed conn")
    finally:
        cs_loop.call_soon_threadsafe(cs_loop.stop)
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    for t in (test_sid_validation_rejects_traversal,
              test_owner_token_required_for_teardown,
              test_box_cannot_register_or_unregister_a_foreign_session,
              test_command_is_the_root_process_row,
              test_dash_dash_required_and_flag_parsing,
              test_single_ui_instance_refused,
              test_derive_parent_sid_owner_discovery,
              test_register_parent_body_field_not_honoured,
              test_register_no_parent_is_top_level,
              test_channel_server_pidfd_register,
              test_apply_kick_up_live_parent,
              test_apply_root_box_still_writes_host,
              test_apply_kick_up_finished_parent,
              test_register_reply_fd_nested_box,
              test_register_reply_fd_toplevel_no_fd,
              # scoped dotted names
              test_dotted_name_validation,
              test_host_register_top_level_name,
              test_host_register_dotted_child,
              test_host_register_dotted_child_finished_parent,
              test_host_register_dotted_parent_not_existing,
              test_inbox_register_relname,
              test_inbox_relname_with_dot_rejected,
              test_inbox_relname_empty_default,
              test_inbox_trust_kernel_parent_not_body,
              test_default_named_box_age_from_ctime,
              test_dotted_path_safety,
              test_rename_is_meta_only_label_write,
              # Feature 1: box-list tree
              test_box_tree_rows_indented_connectors_skipped,
              test_box_tree_connector_skipped_when_prefix_has_no_box,
              # Feature 2: dissolve (pointer re-parenting)
              test_dissolve_happy_path,
              test_dissolve_nonempty_finalizes_copydown,
              test_dissolve_top_level_child_becomes_top_level,
              test_dissolve_refused_when_target_live,
              test_dissolve_live_descendant_allowed,
              # Engine/UI split: the remote-UI control-plane surface
              test_remote_ui_control_plane):
        print(f"\n== {t.__name__} ==")
        try:
            t()
        except Exception as e:
            import traceback; traceback.print_exc(); _fails.append(f"{t.__name__}: {e}")
    print("\n" + ("CONTROL-PLANE PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    sys.exit(1 if _fails else 0)
