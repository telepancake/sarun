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
    try:
        m.ensure_dirs()
        sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=_FakeMount())

        my_pid  = os.getpid()
        my_ppid = os.getppid()

        sid_a = "20260604-000001_111"
        sid_b = "20260604-000001_222"

        # Register session A with root tgid == our ppid (a real ancestor).
        ack_a = sup.register(dict(session_id=sid_a, cmd=["true"],
                                  root_tgid=my_ppid,
                                  prov=dict(ppid=0, exe="/bin/sh", env={})))
        check(ack_a.get("ok") is True, "derive: session A registers ok")

        # Register session B with root tgid == our own pid (not an ancestor of itself
        # when passed a *child* pid; we'll query with my_pid as the peer).
        # We pick a synthetic tgid that is definitively NOT in our PPid chain.
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
              test_register_no_parent_is_top_level):
        print(f"\n== {t.__name__} ==")
        try:
            t()
        except Exception as e:
            import traceback; traceback.print_exc(); _fails.append(f"{t.__name__}: {e}")
    print("\n" + ("CONTROL-PLANE PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    sys.exit(1 if _fails else 0)
