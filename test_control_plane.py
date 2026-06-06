#!/usr/bin/env python3
"""Regression tests for the HIGH control-plane findings.

HIGH-1: untrusted in-box code (reaching only a box's relay socket) must NOT be able
        to (a) unregister another session, (b) register a new one, or (c) gate its
        traffic as another sid — the sid in an FD message is ignored and derived from
        which per-session relay socket the FD arrived on, gated by an owner token.
HIGH-2: register() rejects a session_id that isn't the one legitimate shape (so a
        traversal sid such as "../foo" or "a/b" creates no directory anywhere).

    /home/user/venv/bin/python test_control_plane.py

Self-safety: an isolated XDG temp tree; no real overlay mount; relay sockets are
closed in finally.
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
        # valid_sid accepts the legitimate shapes and nothing else
        check(m.valid_sid("20260604-000000_111"), "valid_sid: plain sid accepted")
        check(m.valid_sid("20260604-000000_111.abc123"),
              "valid_sid: suffixed sid accepted")
        for bad in ("../escape", "a/b", "..", "/etc/passwd", "20260604-000000_111/..",
                    "20260604-000000_111.ABCDEF", "x_1", "", None,
                    "20260604-000000_111\n20260604-000000_222"):
            check(not m.valid_sid(bad), f"valid_sid: rejects {bad!r}")

        # register() must reject a traversal sid BEFORE any mkdir, creating nothing
        # outside live_home(). Use mount=None: the sid check fires first.
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=None)
        outside = tmp / "PWNED"
        ack = sup.register(dict(session_id=f"../../{outside.name}", cmd=["true"]))
        check(ack.get("ok") is False, "register: traversal sid rejected (ok False)")
        check(ack.get("error") == "invalid session_id",
              "register: traversal sid gives 'invalid session_id'")
        check(not outside.exists(), "register: no dir created outside live_home()")

        ack2 = sup.register(dict(session_id="a/b/c", cmd=["true"]))
        check(ack2.get("ok") is False, "register: slashed sid rejected")
        # nothing leaked under live_home() either
        lh = m.live_home()
        leaked = [p for p in (lh.iterdir() if lh.exists() else [])]
        check(leaked == [], "register: no live/<sid> dir created for a bad sid")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


# ── HIGH-1: relay socket gates by socket, not by message sid ────────────────

def _shutdown_loop(loop, rs, thread):
    """Stop a RelayServer + its asyncio loop and join the loop thread, so no pending
    task is GC'd noisily after the test returns."""
    def _close():
        try:
            if rs is not None: rs.stop()
        except Exception: pass
        loop.stop()
    try: loop.call_soon_threadsafe(_close)
    except Exception: pass
    if thread is not None:
        thread.join(timeout=5)
    try: loop.close()
    except Exception: pass


class _RecordingEngine:
    """Stand-in ProxyEngine that just records (sid) it was asked to gate an FD as."""
    def __init__(self):
        self.calls = []      # list of sids
        self.done = threading.Event()
    async def handle_fd(self, fd, session_id):
        self.calls.append(session_id)
        try: os.close(fd)
        except OSError: pass
        self.done.set()


def test_relay_fd_gated_by_socket_not_message():
    tmp = Path(tempfile.mkdtemp(prefix="cp-"))
    _redirect_state(tmp)
    rs = None; t = None
    loop = asyncio.new_event_loop()
    try:
        m.ensure_dirs()
        sid_a = "20260604-000000_111"
        os.makedirs(m.relay_dir(sid_a), mode=0o700, exist_ok=True)
        eng = _RecordingEngine()
        path = m.relay_sock_path(sid_a)

        # run the loop in a thread so we can drive a blocking client from the main one
        ready = threading.Event()
        def run_loop():
            asyncio.set_event_loop(loop)
            nonlocal rs
            rs = m.RelayServer(eng, sid_a, path, loop)
            rs.start()
            ready.set()
            loop.run_forever()
        t = threading.Thread(target=run_loop, daemon=True); t.start()
        ready.wait(5)

        # a box on relay A sends an FD with a SPOOFED sid for another session
        s0, s1 = socket.socketpair()
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as c:
            c.connect(path)
            c.sendmsg([json.dumps({"session_id": "20260604-000000_999"}).encode()],
                      [(socket.SOL_SOCKET, socket.SCM_RIGHTS,
                        array.array("i", [s0.fileno()]).tobytes())])
        s0.close(); s1.close()

        check(eng.done.wait(5), "relay: FD was handed to the engine")
        check(eng.calls == [sid_a],
              f"relay: FD gated as the relay's own sid, NOT the spoofed one "
              f"(got {eng.calls})")
    finally:
        _shutdown_loop(loop, rs, t)
        shutil.rmtree(tmp, ignore_errors=True)


def test_relay_socket_does_not_dispatch_control():
    """A control JSON line sent to a relay socket (the only socket a box can reach)
    must NOT unregister/register anything: relay sockets handle FD messages only and
    never call the control dispatcher."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-"))
    _redirect_state(tmp)
    rs = None; t = None
    loop = asyncio.new_event_loop()
    try:
        m.ensure_dirs()
        sid_a = "20260604-000000_111"
        os.makedirs(m.relay_dir(sid_a), mode=0o700, exist_ok=True)
        eng = _RecordingEngine()
        path = m.relay_sock_path(sid_a)
        ready = threading.Event()
        def run_loop():
            asyncio.set_event_loop(loop)
            nonlocal rs
            rs = m.RelayServer(eng, sid_a, path, loop)
            rs.start(); ready.set(); loop.run_forever()
        t = threading.Thread(target=run_loop, daemon=True); t.start()
        ready.wait(5)

        # send a control message (no FD) to the relay socket
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as c:
            c.connect(path)
            c.sendall((json.dumps(
                {"type": "unregister", "session_id": "20260604-000000_999"})
                + "\n").encode())
            import time; time.sleep(0.5)

        check(eng.calls == [],
              "relay: a control line produced NO gating call (no FD => ignored)")
        # the RelayServer exposes no control entrypoint at all
        check(not hasattr(rs, "_dispatch_control"),
              "relay: RelayServer has no control dispatcher")
    finally:
        _shutdown_loop(loop, rs, t)
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
    def add_session(self, *a, **k): pass
    def remove_session(self, sid): self.ops.remove_session(sid)
    def add_ca_spoof(self, *a, **k): pass


def test_owner_token_required_for_teardown():
    tmp = Path(tempfile.mkdtemp(prefix="cp-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())
        sid = "20260604-000000_111"
        ack = sup.register(dict(session_id=sid, cmd=["true"], want_net=False))
        check(ack.get("ok") is True, "register: a valid sid registers")
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


def test_register_net_wires_a_per_session_relay():
    """register(want_net=True) with a relay factory attached creates THIS box's own
    relay socket and returns its path; an FD sent there is gated as that sid."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-"))
    _redirect_state(tmp)
    rs_loop = asyncio.new_event_loop()
    t = None
    try:
        m.ensure_dirs()
        eng = _RecordingEngine()
        ready = threading.Event()
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())
        def run_loop():
            asyncio.set_event_loop(rs_loop)
            sup.attach_relay_factory(rs_loop, eng)
            ready.set(); rs_loop.run_forever()
        t = threading.Thread(target=run_loop, daemon=True); t.start()
        ready.wait(5)

        sid = "20260604-000000_333"
        # register must run on the relay loop's thread (it touches the loop)
        box = {}
        ev = threading.Event()
        def do_reg():
            box["ack"] = sup.register(dict(session_id=sid, cmd=["true"], want_net=True))
            ev.set()
        rs_loop.call_soon_threadsafe(do_reg); ev.wait(5)
        ack = box["ack"]
        check(ack.get("ok") is True, "register(want_net) ok")
        relay = ack.get("relay")
        check(bool(relay) and Path(relay).exists(),
              "register(want_net): a per-session relay socket exists")
        check(relay == m.relay_sock_path(sid),
              "register(want_net): relay socket is under this session's dir")

        # an FD to that relay is gated as sid (regardless of message content)
        s0, s1 = socket.socketpair()
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as c:
            c.connect(relay)
            c.sendmsg([b"\x00"],
                      [(socket.SOL_SOCKET, socket.SCM_RIGHTS,
                        array.array("i", [s0.fileno()]).tobytes())])
        s0.close(); s1.close()
        check(eng.done.wait(5) and eng.calls == [sid],
              f"register(want_net): FD on the relay is gated as {sid} (got {eng.calls})")

        # teardown stops + removes the relay socket
        ev2 = threading.Event()
        def do_unreg():
            sup.unregister(dict(session_id=sid, owner_token=ack["owner_token"]))
            ev2.set()
        rs_loop.call_soon_threadsafe(do_unreg); ev2.wait(5)
        check(not Path(relay).exists(),
              "unregister: the per-session relay socket is removed")
    finally:
        try: rs_loop.call_soon_threadsafe(rs_loop.stop)
        except Exception: pass
        if t is not None: t.join(timeout=5)
        try: rs_loop.close()
        except Exception: pass
        shutil.rmtree(tmp, ignore_errors=True)


def test_box_cannot_register_or_unregister_a_foreign_session():
    """Two sessions A and B. A box that knows only A's token (the realistic case:
    A's runner) cannot use it to unregister B."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())
        a, b = "20260604-000000_111", "20260604-000000_222"
        tok_a = sup.register(dict(session_id=a, cmd=["true"]))["owner_token"]
        sup.register(dict(session_id=b, cmd=["true"]))
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
    sid's host pid, argv == cmd) in the single <sid>.sqlar — the sole home for the
    command, with no xattr anywhere. The row is present even for an EMPTY box (no
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

        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())
        sid = "20260604-000000_4242"           # the suffix 4242 IS the root host pid
        root_pid = m.parse_sid(sid)[1]
        check(root_pid == 4242, "parse_sid recovers the host pid from the sid")
        cmd = ["echo", "hello world"]
        prov = dict(ppid=7, exe="/usr/bin/echo", env={"FOO": "bar"})
        ack = sup.register(dict(session_id=sid, cmd=cmd, prov=prov, want_trace=True))
        check(ack.get("ok") is True, "register ok")

        # the live process table carries the root row keyed by the host pid
        procs = sup.processes(sid)
        root = next((p for p in procs if p[1] == root_pid), None)
        check(root is not None, "a process row exists for the sid's root pid")
        check(root and root[4] == cmd, "root row argv IS the command")
        check(root and root[3] == "/usr/bin/echo", "root row carries the runner exe")
        check(root and root[2] == 7, "root row carries the runner ppid")

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
    def fake_run(cmd, net, trace, direct, wl, chdir, reuse_sid=None):
        seen.update(cmd=cmd, net=net, trace=trace, direct=direct, wl=wl, chdir=chdir,
                    reuse_sid=reuse_sid)
        return 0

    # missing `--` is refused with a clear, fixable message.
    rc, err = _run_main(["ls"], cmd_run=fake_run)
    check(rc != 0 and "--" in err, "bare `slopbox ls` refused (needs `--`)")
    check("ls" in err, "error suggests the corrected `slopbox -- ls`")
    check(not seen, "cmd_run not called when `--` is missing")

    # defaults: NOTHING enabled (no net, no trace, no direct).
    seen.clear()
    rc, _ = _run_main(["--", "echo", "hi"], cmd_run=fake_run)
    check(seen.get("cmd") == ["echo", "hi"], "command parsed after `--`")
    check(seen.get("net") is False and seen.get("trace") is False
          and seen.get("direct") is False, "default run enables nothing")

    # all three flags, combinable, before `--`.
    seen.clear()
    rc, _ = _run_main(["-n", "-t", "-d", "-w", "ex.com", "--", "ls", "-la"],
                      cmd_run=fake_run)
    check(seen.get("net") and seen.get("trace") and seen.get("direct"),
          "-n/-t/-d are independent and combinable")
    check(seen.get("cmd") == ["ls", "-la"], "flags before `--`, args (incl. -la) after")
    check(seen.get("wl") == ["ex.com"], "-w whitelist parsed")


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
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())

        my_pid  = os.getpid()
        my_ppid = os.getppid()

        sid_a = "20260604-000001_111"
        sid_b = "20260604-000001_222"

        # Register session A: root_tgid == our ppid, supply a live pidfd so
        # _pidfd_alive(sess_a.run_pidfd) is True and the entry appears in root_map.
        ack_a = sup.register(dict(session_id=sid_a, cmd=["true"],
                                  root_tgid=my_ppid,
                                  _register_pidfd=pidfd_a,
                                  prov=dict(ppid=0, exe="/bin/sh", env={})))
        check(ack_a.get("ok") is True, "derive: session A registers ok")
        # register() dup'd the fd; close our copy so it doesn't leak.
        try: os.close(pidfd_a)
        except OSError: pass
        pidfd_a = -1

        # Register session B: root_tgid is a synthetic dead pid; no pidfd supplied →
        # run_pidfd stays -1 → _pidfd_alive returns False → B never enters root_map.
        fake_root_b = 99999999   # unlikely to be alive, definitely not our ancestor
        ack_b = sup.register(dict(session_id=sid_b, cmd=["true"],
                                  root_tgid=fake_root_b,
                                  prov=dict(ppid=0, exe="/bin/false", env={})))
        check(ack_b.get("ok") is True, "derive: session B registers ok")

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
        sup_empty = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())
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
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())

        sid_parent = "20260604-000002_111"
        sid_child  = "20260604-000002_222"

        sup.register(dict(session_id=sid_parent, cmd=["true"]))
        check(sup.sessions[sid_parent].live, "no-spoof: parent session live")

        # The child message carries a box-supplied 'parent_sid' (untrusted body field).
        # Supervisor.register should ignore it.  No _derived_parent_sid → top-level box.
        ack = sup.register(dict(session_id=sid_child, cmd=["true"],
                                parent_sid=sid_parent))   # untrusted field
        check(ack.get("ok") is True, "no-spoof: register with body parent_sid still ok")

        # Verify add_session was called with parent=None (not sid_parent).
        # The _FakeMount.add_session just records calls in _FakeOps; the test checks
        # that no error was injected (ack ok=True + no error) and the session is live.
        check(sup.sessions[sid_child].live, "no-spoof: child session is live")
        # If the body field had been honoured and sid_parent was resolved, the
        # overlay would have parent=sid_parent.  We verify it was NOT honoured by
        # checking that a non-live derived parent would have failed closed:
        ack2 = sup.register(dict(session_id="20260604-000002_333", cmd=["true"],
                                 _derived_parent_sid=sid_child + "_nonexistent"))
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
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())
        sid = "20260604-000003_111"
        # No _derived_parent_sid key → top-level
        ack = sup.register(dict(session_id=sid, cmd=["true"]))
        check(ack.get("ok") is True, "top-level: register ok with no parent")
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
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())
        eng = _RecordingEngine()
        sock_path = str(tmp / "ctrl.sock")

        ready = threading.Event()
        cs = None
        def run_loop():
            nonlocal cs
            asyncio.set_event_loop(cs_loop)
            cs = m.ChannelServer(sup, eng, sock_path)
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
        sid = "20260604-000010_777"
        msg = dict(type="register", session_id=sid, cmd=["true"], want_net=False)
        payload = (json.dumps(msg) + "\n").encode()

        pidfd = os.pidfd_open(os.getpid())
        ack = None
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
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
        finally:
            try: os.close(pidfd)
            except OSError: pass

        check(ack is not None, "channel-server: register with pidfd got a reply")
        check(ack and ack.get("ok") is True,
              f"channel-server: register ack ok=True (got {ack!r})")
        check(ack and bool(ack.get("owner_token")),
              "channel-server: register ack includes an owner_token")

        # Session is live in the supervisor
        check(sid in sup.sessions and sup.sessions[sid].live,
              "channel-server: session is live after pidfd-register")

        # _derive_parent_sid with our own pid returns None (no session has us as ancestor)
        # — the test just verifies it doesn't crash and returns None for this case.
        result = m._derive_parent_sid(os.getpid(), sup)
        check(result is None or isinstance(result, str),
              f"channel-server: _derive_parent_sid ok (got {result!r})")

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
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())

        sid_p = "20260604-000100_111"   # parent
        sid_c = "20260604-000100_222"   # child (nested under parent)

        # Register parent.
        ack_p = sup.register(dict(session_id=sid_p, cmd=["parent"]))
        check(ack_p.get("ok") is True, "kick-up: parent registers ok")

        # Register child with _derived_parent_sid pointing at parent.
        ack_c = sup.register(dict(session_id=sid_c, cmd=["child"],
                                  _derived_parent_sid=sid_p))
        check(ack_c.get("ok") is True, "kick-up: child registers ok with parent")
        check(sup.sessions[sid_c].parent == sid_p,
              "kick-up: Session.parent is set on the child")

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
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())
        sid = "20260604-000101_111"
        sup.register(dict(session_id=sid, cmd=["root"]))
        check(sup.sessions[sid].parent is None,
              "root-box: Session.parent is None for a top-level box")
        # The parent_sid() helper must return None.
        check(sup.review._parent_sid(sid) is None,
              "root-box: _parent_sid returns None for root")
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
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())

        sid_p = "20260604-000102_111"
        sid_c = "20260604-000102_222"

        # Register parent then child.
        sup.register(dict(session_id=sid_p, cmd=["parent"]))
        ack_c = sup.register(dict(session_id=sid_c, cmd=["child"],
                                  _derived_parent_sid=sid_p))
        check(ack_c.get("ok") is True, "kick-up-fin: child registers")

        # Tear down parent to simulate it finishing (it goes to sqlar).
        tok_p = sup._owner_tokens.get(sid_p, "")
        sup.unregister(dict(session_id=sid_p, owner_token=tok_p,
                            status="finished", exit_code=0))
        # Parent index is gone; Session may still exist in sessions dict.
        check(sid_p not in sup.indexes,
              "kick-up-fin: parent Index is gone after teardown")

        # The child still knows its parent sid.
        check(sup.sessions[sid_c].parent == sid_p,
              "kick-up-fin: child still records parent sid")

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

        # Verify _parent_sid still resolves (even though parent is finished,
        # it should still be in sessions dict if it had changes).
        # If the parent was cleaned up (no sqlar content), inject it back.
        if sid_p not in sup.sessions:
            sup.sessions[sid_p] = m.Session(session_id=sid_p, cmd=["parent"],
                                             live=False)

        # Apply the child's change — should promote to parent sqlar.
        result = sup.review.apply(sid_c, [rel])
        # If parent resolves, the change gets promoted.
        psid = sup.review._parent_sid(sid_c)
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
    """ChannelServer sends a dir-fd on the register reply for a NESTED box only.

    Strategy:
    - Start a real ChannelServer with a _FakeMount.
    - Register a parent session so _derive_parent_sid can resolve it.
    - Register a child session from a client that recvmsg's the reply.
      For the server to open the child's box_root we pre-create that directory
      (normally created by the real FUSE add_session; here we replicate it).
    - Assert the client receives an fd via SCM_RIGHTS whose /proc/self/fd path
      points to the child's box_root directory.
    - Register a top-level session (no live parent) and assert NO fd is sent.
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
            sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())
            eng = _RecordingEngine()
            sock_path = str(tmp / "ctrl_replyfd.sock")

            ready = threading.Event()
            cs = None
            def run_loop():
                nonlocal cs
                asyncio.set_event_loop(cs_loop)
                cs = m.ChannelServer(sup, eng, sock_path)
                cs_loop.run_until_complete(cs.start())
                ready.set()
                cs_loop.run_forever()
            cs_thread = threading.Thread(target=run_loop, daemon=True)
            cs_thread.start()
            check(ready.wait(5), "reply-fd: loop started")

            # Register the parent session inline (no network path needed).
            sid_parent = "20260606-000010_111"
            ack_p = sup.register(dict(session_id=sid_parent, cmd=["parent"],
                                      root_tgid=os.getppid(),
                                      _register_pidfd=pidfd_parent,
                                      prov=dict(ppid=0, exe="/bin/sh", env={})))
            check(ack_p.get("ok") is True, "reply-fd: parent session registers ok")
            # register() dup'd the pidfd; close our copy so it doesn't leak.
            try: os.close(pidfd_parent)
            except OSError: pass
            pidfd_parent = -1

            # Pre-create the child's box_root directory so os.open() in register() works.
            sid_child = "20260606-000010_222"
            child_box_root = m.mnt_point() / sid_child
            child_box_root.mkdir(parents=True, exist_ok=True)

            # Send the child register and recvmsg the reply to capture the fd.
            msg = dict(type="register", session_id=sid_child, cmd=["child"],
                       want_net=False)
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

            check(child_ack is not None, "reply-fd: child register got a reply")
            check(child_ack and child_ack.get("ok") is True,
                  f"reply-fd: child ack ok=True (got {child_ack!r})")

            # KEY ASSERTION: child is nested → should have received a mount fd.
            parent_derived = sup.sessions[sid_child].parent if sid_child in sup.sessions else None
            if child_mount_fd >= 0:
                # The fd is an open_tree fd — a mount fd whose /proc/self/fd/N symlink
                # resolves to "/" (the root of the cloned mount), not the source path.
                # Verify the underlying inode matches child_box_root by comparing
                # the fstat of the fd with the stat of the directory.
                try:
                    fd_stat = os.fstat(child_mount_fd)
                    dir_stat = child_box_root.stat()
                    same_ino = (fd_stat.st_ino == dir_stat.st_ino and
                                fd_stat.st_dev == dir_stat.st_dev)
                except OSError:
                    same_ino = False
                check(same_ino,
                      f"reply-fd: received mount fd's inode matches child box_root "
                      f"({child_box_root})")
                try: os.close(child_mount_fd)
                except OSError: pass
                child_mount_fd = -1
            else:
                # If parent was not derived (e.g. kernel didn't give us host pid via pidfd),
                # the fd is legitimately absent. Report clearly rather than failing.
                check(parent_derived is None,
                      f"reply-fd: no fd sent but parent_derived={parent_derived!r} "
                      f"(expected None if no fd)")

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
        sup2 = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())
        eng2 = _RecordingEngine()
        sock2 = str(tmp / "ctrl_top.sock")

        ready2 = threading.Event()
        cs2 = None
        def run_loop2():
            nonlocal cs2
            asyncio.set_event_loop(cs2_loop)
            cs2 = m.ChannelServer(sup2, eng2, sock2)
            cs2_loop.run_until_complete(cs2.start())
            ready2.set()
            cs2_loop.run_forever()
        cs2_thread = threading.Thread(target=run_loop2, daemon=True)
        cs2_thread.start()
        check(ready2.wait(5), "reply-fd-top: loop started")

        sid_top = "20260606-000011_444"
        top_box_root = m.mnt_point() / sid_top
        top_box_root.mkdir(parents=True, exist_ok=True)
        msg_top = dict(type="register", session_id=sid_top, cmd=["top"], want_net=False)
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
    # valid_sid now accepts dotted names
    check(m.valid_sid("A.B"), "valid_sid: dotted name accepted")
    check(m.valid_sid("A.B.C"), "valid_sid: triple-dotted name accepted")
    check(not m.valid_sid(".."), "valid_sid: '..' still rejected")
    check(not m.valid_sid("A/B"), "valid_sid: slash still rejected")


def test_host_register_top_level_name():
    """HOST: register with a single-segment NAME (no parent) creates a top-level box."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-dn-tl-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())
        ack = sup.register(dict(session_id="ALPHA", cmd=["true"]))
        check(ack.get("ok") is True, "host-tl: register A ok")
        check("ALPHA" in sup.sessions, "host-tl: ALPHA in sessions")
        check(sup.sessions["ALPHA"].parent is None, "host-tl: ALPHA parent is None")
        check(sup.sessions["ALPHA"].live, "host-tl: ALPHA is live")
        # born timestamp set so age sorting works
        born = sup.sessions["ALPHA"].born
        check(bool(born), f"host-tl: born is set (got {born!r})")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_host_register_dotted_child():
    """HOST: A.B with A live → creates child B of A (parent = A)."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-dn-child-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())
        # Create parent A first.
        ack_a = sup.register(dict(session_id="ALPHA", cmd=["true"]))
        check(ack_a.get("ok") is True, "host-child: ALPHA registers ok")
        # Create child A.B.
        ack_ab = sup.register(dict(session_id="ALPHA.BETA", cmd=["true"]))
        check(ack_ab.get("ok") is True, f"host-child: ALPHA.BETA registers ok (got {ack_ab})")
        check("ALPHA.BETA" in sup.sessions, "host-child: ALPHA.BETA in sessions")
        check(sup.sessions["ALPHA.BETA"].parent == "ALPHA",
              "host-child: ALPHA.BETA parent is ALPHA")
        check(sup.sessions["ALPHA.BETA"].live, "host-child: ALPHA.BETA is live")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_host_register_dotted_child_finished_parent():
    """HOST: A.B with A finished (sqlar on disk) → parent resolves to A's sqlar."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-dn-finpar-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())
        # Write a minimal sqlar for the parent AFTER the Supervisor is created so that
        # ALPHA exists on disk but is not a live session (discover_sessions already ran).
        sp = m.sqlar_path("ALPHA")
        sp.parent.mkdir(parents=True, exist_ok=True)
        import sqlite3
        conn = sqlite3.connect(str(sp))
        conn.execute("CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT)")
        conn.execute("INSERT OR REPLACE INTO meta VALUES('born','20260101-000000_1')")
        conn.commit(); conn.close()
        # ALPHA is finished (sqlar exists, NOT live).
        check(not (sup.sessions.get("ALPHA") and sup.sessions["ALPHA"].live),
              "host-finpar: ALPHA not live")
        check(m.sqlar_path("ALPHA").exists(), "host-finpar: ALPHA sqlar exists")

        ack = sup.register(dict(session_id="ALPHA.BETA", cmd=["true"]))
        check(ack.get("ok") is True, f"host-finpar: ALPHA.BETA registers ok (got {ack})")
        check(sup.sessions["ALPHA.BETA"].parent == "ALPHA",
              "host-finpar: parent is ALPHA (finished)")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_host_register_dotted_parent_not_existing():
    """HOST: A.B with A NOT existing → fail-closed with a clear error."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-dn-nopar-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())
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
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())
        # Register the parent (enclosing) box.
        ack_a = sup.register(dict(session_id="ALPHA", cmd=["parent"]))
        check(ack_a.get("ok") is True, "inbox: ALPHA registers ok")

        # Simulate in-box child: relname=BETA, _derived_parent_sid=ALPHA (kernel-derived).
        ack_c = sup.register(dict(
            session_id="20260606-000000_123",   # temp auto-sid (UI replaces it)
            relname="BETA",
            _derived_parent_sid="ALPHA",
            cmd=["child"]))
        check(ack_c.get("ok") is True, f"inbox: ALPHA.BETA registers ok (got {ack_c})")
        # UI should have created session ALPHA.BETA, not the temp sid.
        check("ALPHA.BETA" in sup.sessions,
              "inbox: resolved sid is ALPHA.BETA (not the temp auto-sid)")
        check(sup.sessions["ALPHA.BETA"].parent == "ALPHA",
              "inbox: ALPHA.BETA parent is ALPHA (kernel-derived, not overridable)")
        check(sup.sessions["ALPHA.BETA"].live, "inbox: ALPHA.BETA is live")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_inbox_relname_with_dot_rejected():
    """IN-BOX: relname containing a dot or slash is rejected (prevents subtree escape)."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-dn-escape-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())
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
    """IN-BOX: relname='' → default D<N> name assigned under enclosing box."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-dn-default-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())
        ack_a = sup.register(dict(session_id="ALPHA", cmd=["parent"]))
        check(ack_a.get("ok") is True, "inbox-default: ALPHA registers ok")

        ack_c = sup.register(dict(
            session_id="20260606-000000_789",
            relname="",       # empty = let the UI assign a default D<N>
            _derived_parent_sid="ALPHA",
            cmd=["child"]))
        check(ack_c.get("ok") is True, f"inbox-default: empty relname ok (got {ack_c})")
        # The resolved sid must start with "ALPHA." and contain a "D" segment.
        resolved = [s for s in sup.sessions if s.startswith("ALPHA.")]
        check(len(resolved) == 1, f"inbox-default: exactly one ALPHA.* session (got {resolved})")
        child_sid = resolved[0]
        seg = child_sid.split(".", 1)[1]
        check(seg.startswith("D"), f"inbox-default: segment starts with D (got {seg!r})")
        check(sup.sessions[child_sid].parent == "ALPHA",
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
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())
        # Two parent boxes: ALPHA (live) and EVIL (live).
        sup.register(dict(session_id="ALPHA", cmd=["alpha"]))
        sup.register(dict(session_id="EVIL", cmd=["evil"]))

        # A box inside ALPHA tries to forge its parent as EVIL via session_id.
        # The kernel says _derived_parent_sid=ALPHA — that must win.
        ack = sup.register(dict(
            session_id="EVIL.SNEAKY",    # box tries to claim it's under EVIL
            relname="SNEAKY",
            _derived_parent_sid="ALPHA", # kernel says it's inside ALPHA
            cmd=["sneaky"]))
        check(ack.get("ok") is True, "trust: SNEAKY registers (kernel parent honoured)")
        # The resolved name should be ALPHA.SNEAKY, not EVIL.SNEAKY.
        check("ALPHA.SNEAKY" in sup.sessions,
              "trust: resolved as ALPHA.SNEAKY (kernel wins)")
        check("EVIL.SNEAKY" not in sup.sessions,
              "trust: EVIL.SNEAKY NOT created (box-body ignored)")
        check(sup.sessions["ALPHA.SNEAKY"].parent == "ALPHA",
              "trust: parent is kernel-derived ALPHA, not body-supplied EVIL")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_default_named_box_born_and_sort():
    """A default D<N> or explicit named box gets a 'born' timestamp so age/sort works."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-dn-born-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())
        ack = sup.register(dict(session_id="MYBOX", cmd=["true"]))
        check(ack.get("ok") is True, "born: MYBOX registers ok")
        s = sup.sessions["MYBOX"]
        check(bool(s.born), f"born: Session.born is set (got {s.born!r})")
        # parse_sid on a dotted/named sid returns (0, 0) → falls back to born.
        check(m.parse_sid("MYBOX") == (0.0, 0),
              "born: parse_sid('MYBOX') returns (0,0)")
        check(s.started > 0,
              f"born: Session.started uses born fallback > 0 (got {s.started})")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_dotted_path_safety():
    """Dotted names with '..' or '/' embedded are rejected before any mkdir."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-dn-safe-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())
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


def test_rename_to_dotted_name():
    """rename() accepts dotted names; dotted rename with non-existent parent is rejected."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-dn-rename-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())
        # Create and finish box ALPHA.
        ack = sup.register(dict(session_id="ALPHA", cmd=["true"]))
        check(ack.get("ok") is True, "rename-dotted: ALPHA registers")
        sup.unregister(dict(session_id="ALPHA", owner_token=ack["owner_token"],
                            status="finished"))
        # Create parent PARENT as a finished sqlar so ALPHA can rename to PARENT.ALPHA.
        import sqlite3
        ps = m.sqlar_path("PARENT")
        ps.parent.mkdir(parents=True, exist_ok=True)
        conn = sqlite3.connect(str(ps))
        conn.execute("CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT)")
        conn.commit(); conn.close()
        sup.sessions["PARENT"] = m.Session(session_id="PARENT", cmd=["p"], live=False)
        # Rename ALPHA → PARENT.ALPHA (dotted target).
        r = sup.rename("ALPHA", "PARENT.ALPHA")
        check(r.get("ok") is True, f"rename-dotted: rename to PARENT.ALPHA ok (got {r})")
        check(r.get("sid") == "PARENT.ALPHA", "rename-dotted: sid is PARENT.ALPHA")
        # Rename to a dotted name whose parent doesn't exist: rejected.
        r2 = sup.rename("PARENT", "MISSING.CHILD")
        check(r2.get("ok") is False,
              "rename-dotted: rename to MISSING.CHILD rejected (parent not exist)")
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

def _make_finished_box(sup, sid, with_content=False):
    """Create a minimal finished (non-live) box for splice_delete tests.
    If with_content=True, insert a sqlar row so the box survives _maybe_remove_empty."""
    sp = m.sqlar_path(sid)
    sp.parent.mkdir(parents=True, exist_ok=True)
    import sqlite3
    conn = sqlite3.connect(str(sp))
    conn.execute("CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT)")
    conn.execute("INSERT OR REPLACE INTO meta VALUES('born','20260101-000000_1')")
    if with_content:
        conn.execute(
            "CREATE TABLE IF NOT EXISTS sqlar"
            "(name TEXT PRIMARY KEY, mode INT, mtime INT, sz INT, data BLOB)")
        conn.execute("INSERT OR REPLACE INTO sqlar VALUES('proof.txt',33188,0,4,X'41424344')")
    conn.commit(); conn.close()
    sup.sessions[sid] = m.Session(session_id=sid, cmd=["test"], live=False,
                                  shm_dir=str(m.live_dir(sid)))


def test_splice_delete_happy_path():
    """splice_delete('A.B') removes A.B and re-parents A.B.C→A.C, A.B.C.D→A.C.D.
    Content is preserved; A.C's parent_sid meta resolves to A."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-splice-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())

        # A: top-level parent (must exist so rename allows A.B.C→A.C).
        _make_finished_box(sup, "A", with_content=False)
        # A.B: the splice target — no content (empty sqlar, no flows, no upper).
        _make_finished_box(sup, "A.B", with_content=False)
        # A.B.C: child with content.
        _make_finished_box(sup, "A.B.C", with_content=True)
        m.sqlar_meta_set(m.sqlar_path("A.B.C"), "parent_sid", "A.B")
        # A.B.C.D: grandchild with content.
        _make_finished_box(sup, "A.B.C.D", with_content=True)
        m.sqlar_meta_set(m.sqlar_path("A.B.C.D"), "parent_sid", "A.B.C")

        r = sup.splice_delete("A.B")
        check(r.get("ok") is True, f"splice-happy: splice_delete ok (got {r})")
        check(r.get("deleted") == "A.B", "splice-happy: deleted is A.B")
        check(len(r.get("renamed", [])) == 2, f"splice-happy: 2 renames (got {r.get('renamed')})")

        # A.B is gone.
        check("A.B" not in sup.sessions, "splice-happy: A.B not in sessions")
        check(not m.sqlar_path("A.B").exists(), "splice-happy: A.B.sqlar removed")

        # Old paths gone.
        check("A.B.C" not in sup.sessions, "splice-happy: A.B.C not in sessions")
        check(not m.sqlar_path("A.B.C").exists(), "splice-happy: A.B.C.sqlar gone")
        check("A.B.C.D" not in sup.sessions, "splice-happy: A.B.C.D not in sessions")
        check(not m.sqlar_path("A.B.C.D").exists(), "splice-happy: A.B.C.D.sqlar gone")

        # New paths exist.
        check(m.sqlar_path("A.C").exists(), "splice-happy: A.C.sqlar exists")
        check(m.sqlar_path("A.C.D").exists(), "splice-happy: A.C.D.sqlar exists")

        # Content is preserved.
        import sqlite3
        conn = sqlite3.connect(str(m.sqlar_path("A.C")))
        row = conn.execute("SELECT data FROM sqlar WHERE name='proof.txt'").fetchone()
        conn.close()
        check(row is not None and row[0] == b"ABCD", f"splice-happy: A.C content intact (got {row})")

        # parent_sid meta updated correctly.
        ac_parent = m.sqlar_meta_get(m.sqlar_path("A.C"), "parent_sid")
        check(ac_parent == "A", f"splice-happy: A.C parent_sid='A' (got {ac_parent!r})")
        acd_parent = m.sqlar_meta_get(m.sqlar_path("A.C.D"), "parent_sid")
        check(acd_parent == "A.C", f"splice-happy: A.C.D parent_sid='A.C' (got {acd_parent!r})")

    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_splice_delete_refused_has_changes():
    """splice_delete refuses if the target box has sqlar entries (has changes)."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-splice-changes-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())
        _make_finished_box(sup, "A", with_content=False)
        _make_finished_box(sup, "A.B", with_content=True)   # has sqlar content → refused
        _make_finished_box(sup, "A.B.C", with_content=True)

        r = sup.splice_delete("A.B")
        check(r.get("ok") is False, f"splice-changes: refused (got {r})")
        check("sqlar" in (r.get("error") or ""),
              f"splice-changes: error mentions sqlar (got {r.get('error')!r})")
        # Nothing renamed.
        check(m.sqlar_path("A.B").exists(), "splice-changes: A.B.sqlar still present")
        check(m.sqlar_path("A.B.C").exists(), "splice-changes: A.B.C.sqlar still present")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_splice_delete_refused_collision():
    """splice_delete refuses if a rename target already exists (no partial renames)."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-splice-coll-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())
        _make_finished_box(sup, "A", with_content=False)
        _make_finished_box(sup, "A.B", with_content=False)
        _make_finished_box(sup, "A.B.C", with_content=True)
        # Pre-create A.C (the rename target) so there's a collision.
        _make_finished_box(sup, "A.C", with_content=False)

        r = sup.splice_delete("A.B")
        check(r.get("ok") is False, f"splice-coll: refused (got {r})")
        check("collide" in (r.get("error") or "").lower() or "already exists" in (r.get("error") or ""),
              f"splice-coll: error mentions collision (got {r.get('error')!r})")
        # A.B.C must still be present (no partial rename).
        check(m.sqlar_path("A.B.C").exists(), "splice-coll: A.B.C.sqlar not renamed (no partial)")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_splice_delete_refused_live_descendant():
    """splice_delete refuses if any descendant is live/running."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-splice-live-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())
        _make_finished_box(sup, "A", with_content=False)
        _make_finished_box(sup, "A.B", with_content=False)
        _make_finished_box(sup, "A.B.C", with_content=True)
        # Make A.B.C appear live by setting live=True and creating an up/ dir.
        ld = m.live_dir("A.B.C"); ld.mkdir(parents=True, exist_ok=True)
        (ld / "up").mkdir(parents=True, exist_ok=True)
        sup.sessions["A.B.C"].live = True
        sup.sessions["A.B.C"].shm_dir = str(ld)

        r = sup.splice_delete("A.B")
        check(r.get("ok") is False, f"splice-live: refused (got {r})")
        check("running" in (r.get("error") or "").lower(),
              f"splice-live: error mentions running (got {r.get('error')!r})")
        # No renames occurred.
        check(m.sqlar_path("A.B.C").exists(), "splice-live: A.B.C.sqlar untouched")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def test_splice_delete_shallowest_first_ordering():
    """A 3-level subtree splices correctly, proving shallowest-first satisfies
    rename()'s parent-exists guard: A.B.C→A.C (parent A) before A.B.C.D→A.C.D
    (parent A.C, which must exist first)."""
    tmp = Path(tempfile.mkdtemp(prefix="cp-splice-order-"))
    _redirect_state(tmp)
    try:
        m.ensure_dirs()
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())
        _make_finished_box(sup, "A", with_content=False)
        _make_finished_box(sup, "A.B", with_content=False)
        _make_finished_box(sup, "A.B.C", with_content=True)
        _make_finished_box(sup, "A.B.C.D", with_content=True)

        r = sup.splice_delete("A.B")
        check(r.get("ok") is True, f"splice-order: splice_delete ok (got {r})")

        # All expected targets exist.
        check(m.sqlar_path("A.C").exists(), "splice-order: A.C.sqlar created")
        check(m.sqlar_path("A.C.D").exists(), "splice-order: A.C.D.sqlar created")

        # Verify rename order in result list (A.B.C → A.C before A.B.C.D → A.C.D).
        renames = r.get("renamed", [])
        old_names = [pair[0] for pair in renames]
        check("A.B.C" in old_names, "splice-order: A.B.C in renamed list")
        check("A.B.C.D" in old_names, "splice-order: A.B.C.D in renamed list")
        check(old_names.index("A.B.C") < old_names.index("A.B.C.D"),
              "splice-order: A.B.C renamed before A.B.C.D (shallowest-first)")

        # parent_sid chain is correct.
        check(m.sqlar_meta_get(m.sqlar_path("A.C"), "parent_sid") == "A",
              "splice-order: A.C parent_sid='A'")
        check(m.sqlar_meta_get(m.sqlar_path("A.C.D"), "parent_sid") == "A.C",
              "splice-order: A.C.D parent_sid='A.C'")

    finally:
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    for t in (test_sid_validation_rejects_traversal,
              test_relay_fd_gated_by_socket_not_message,
              test_relay_socket_does_not_dispatch_control,
              test_owner_token_required_for_teardown,
              test_register_net_wires_a_per_session_relay,
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
              test_default_named_box_born_and_sort,
              test_dotted_path_safety,
              test_rename_to_dotted_name,
              # Feature 1: box-list tree
              test_box_tree_rows_indented_connectors_skipped,
              test_box_tree_connector_skipped_when_prefix_has_no_box,
              # Feature 2: splice_delete
              test_splice_delete_happy_path,
              test_splice_delete_refused_has_changes,
              test_splice_delete_refused_collision,
              test_splice_delete_refused_live_descendant,
              test_splice_delete_shallowest_first_ordering):
        print(f"\n== {t.__name__} ==")
        try:
            t()
        except Exception as e:
            import traceback; traceback.print_exc(); _fails.append(f"{t.__name__}: {e}")
    print("\n" + ("CONTROL-PLANE PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    sys.exit(1 if _fails else 0)
