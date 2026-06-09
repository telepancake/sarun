#!/usr/bin/env python3
"""The `-n` networking path, in process. mitmproxy IS importable in this venv, so
this drives the real ProxyEngine / Master (the `-n` path) end to end on loopback and
validates the HOST-namespace invariant: EVERY tgid persisted in a per-box <sid>.sqlar
must be a HOST pid-namespace pid.

    /home/user/venv/bin/python test_net_flow.py

Coverage, all against the box's SINGLE <sid>.sqlar:
  A. a live HTTP request, proxied through the in-process engine, lands a flow row
     (correct host/port/status/action).
  B. the pidfd -> host-pid -> read_provenance -> _CURRENT_PROV/log_flow chain (the
     EXACT chain RelayServer._handle runs) tags a flow with the HOST pid of a real
     local child process, plus its real exe/argv/env -- no box-namespace pid is ever
     persisted.
  C. dedup: two flows from one process share one process+env row.
  D. S4: a flow whose write cannot be dropped silently.
  E. peer-gone: a pidfd of an already-exited process, and the no-pidfd case, both
     leave the flow recorded but UNTAGGED -- no bogus process row invented.

Self-safety: isolated XDG temp tree; loopback only; no external internet.
"""
import os, sys, asyncio, socket, threading, tempfile, shutil, sqlite3, json, time
import subprocess
import http.server
from pathlib import Path
from importlib.machinery import SourceFileLoader

# Isolate state BEFORE import: MITM_CONFDIR / state_home are bound at import time.
_TMP = Path(tempfile.mkdtemp(prefix="netflow-"))
os.environ["XDG_STATE_HOME"]  = str(_TMP / "state")
os.environ["XDG_RUNTIME_DIR"] = str(_TMP / "run")
os.environ["XDG_CONFIG_HOME"] = str(_TMP / "config")
os.environ["XDG_DATA_HOME"]   = str(_TMP / "data")

m = SourceFileLoader("slopbox", "/home/user/sarun/sarun").load_module()

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


def _tcp_pair():
    """A connected loopback TCP socket pair (the FD the box hands over is a real TCP
    socket; mitmproxy reads its peername, so AF_UNIX socketpair won't do)."""
    lsn = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    lsn.bind(("127.0.0.1", 0)); lsn.listen(1)
    client = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    client.connect(lsn.getsockname())
    box_side, _ = lsn.accept(); lsn.close()
    return box_side, client


def _tiny_upstream():
    """A loopback HTTP server the engine fetches from in the UI's network context."""
    class H(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            body = b"hello-from-upstream\n"
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        def log_message(self, *a): pass
    srv = http.server.HTTPServer(("127.0.0.1", 0), H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv


def _mk_supervisor(sid, tracing):
    """A Supervisor + a live Index for `sid`, with an allow-all rule and a session so
    approval_request/note_traffic/log_flow all resolve. Returns (sup, idx, sp)."""
    rules = m.Rules(_TMP / "rules.txt")     # empty permanent ruleset
    sup = m.Supervisor(rules)
    backing = m.live_dir(sid); (backing / "up").mkdir(parents=True, exist_ok=True)
    idx = m.Index(backing); idx.set_env_capture(tracing)
    sup.indexes[sid] = idx
    sup.sessions[sid] = m.Session(
        session_id=sid, box_id=int(sid), cmd=["curl", "x"], shm_dir=str(backing),
        live=True, sess_rules=[m.Rule.parse("allow host:*")])
    return sup, idx, m.sqlar_path(sid)


def _resolve_prov_via_pidfd(pidfd: int) -> dict:
    """Run the EXACT host-pid resolution RelayServer._handle does for a pidfd:
    pidfd -> /proc/self/fdinfo Pid (host namespace) -> read_provenance(host_pid) ->
    prov dict (host tgid/ppid/exe/argv/env). {"tgid":0} if the peer is gone."""
    prov = {"tgid": 0}
    if pidfd >= 0:
        host_pid = m._host_pid_from_pidfd(pidfd)
        if host_pid > 0:
            info = m.read_provenance(host_pid, full_env=True)
            ppid_tgid = m.tgid_of(info["ppid"]) if info.get("ppid") else 0
            prov = {"tgid": m.tgid_of(host_pid), "ppid": ppid_tgid,
                    "exe": info["exe"], "argv": info["argv"], "env": info["env"]}
    return prov


def _spawn_child():
    """A real local child process whose HOST pid we know. `cat` blocking on a pipe so
    it stays alive (and reaped cleanly later)."""
    r, w = os.pipe()
    p = subprocess.Popen(["cat"], stdin=r, stdout=subprocess.DEVNULL)
    os.close(r)
    return p, w


def test_live_http_flow_through_engine():
    """A + B: a real proxied loopback GET drives the engine and lands a flow row,
    tagged via the REAL pidfd->host-pid->read_provenance chain with the HOST pid of a
    live local child (its real exe/argv/env)."""
    sid = "4101"
    up = _tiny_upstream(); uhost, uport = up.server_address
    sup, idx, sp = _mk_supervisor(sid, tracing=True)

    # A real child whose HOST pid we will recover from its pidfd. In this environment
    # there is no separate box namespace, so the pidfd's host pid == child.pid -- which
    # is exactly what the box->pidfd->UI chain must yield (a HOST pid).
    child, child_w = _spawn_child()
    try:
        pidfd = os.pidfd_open(child.pid, 0)
    except (OSError, AttributeError) as e:
        check(False, f"os.pidfd_open available ({e})"); pidfd = -1
    prov = _resolve_prov_via_pidfd(pidfd)
    if pidfd >= 0:
        os.close(pidfd)

    check(prov["tgid"] == child.pid,
          "prov tgid is the child's HOST pid (resolved via pidfd, not a box pid)")
    real_exe = os.path.realpath("/proc/%d/exe" % child.pid)
    check(prov.get("exe") == real_exe, "prov exe is the child's real /proc exe")
    check(prov.get("argv") and prov["argv"][0].endswith("cat"),
          "prov argv is the child's real argv")
    check(isinstance(prov.get("env"), dict) and len(prov["env"]) > 0,
          "prov env captured from the child's host /proc/environ")

    loop = asyncio.new_event_loop()
    engine = m.ProxyEngine(sup, loop, schedule=lambda c: asyncio.ensure_future(c, loop=loop))
    check(engine.ready, "ProxyEngine/Master constructed (the -n path is live)")
    if not engine.ready:
        up.shutdown(); child.terminate(); child.wait(); os.close(child_w); return

    box_side, client = _tcp_pair()

    async def drive():
        # Tag this connection with the host-resolved provenance, exactly as the relay
        # handler does after recvmsg + pidfd resolution.
        m._CURRENT_PROV.set(prov)
        fd = os.dup(box_side.fileno())
        task = asyncio.ensure_future(engine.handle_fd(fd, sid))
        req = (f"GET http://{uhost}:{uport}/ HTTP/1.1\r\n"
               f"Host: {uhost}:{uport}\r\nConnection: close\r\n\r\n").encode()
        await loop.sock_sendall(client, req)
        data = b""
        while b"hello-from-upstream" not in data:
            try:
                chunk = await asyncio.wait_for(loop.sock_recv(client, 65536), timeout=10)
            except asyncio.TimeoutError:
                break
            if not chunk: break
            data += chunk
        task.cancel()
        try: await task
        except (asyncio.CancelledError, Exception): pass
        return data

    client.setblocking(False)
    resp = loop.run_until_complete(asyncio.wait_for(drive(), timeout=30))
    loop.run_until_complete(asyncio.sleep(0))
    loop.close()
    box_side.close(); client.close(); up.shutdown()
    child.terminate(); child.wait(); os.close(child_w)

    check(b"hello-from-upstream" in resp, "proxied loopback GET returned upstream body")

    flows = m.flows_list(sp)
    check(len(flows) >= 1, "a flow row landed in the box's single sqlar")
    if not flows: idx.close(); return
    fl = flows[-1]
    check(fl["host"] == uhost and fl["port"] == uport, "flow recorded the upstream host:port")
    check(fl["status"] == 200, "flow recorded the 200 status")
    check(fl["action"] == "allow", "flow recorded the allow decision")

    # The process row tagged on the flow must carry the child's HOST pid + real fields.
    pid_col = fl["process_id"]
    check(pid_col is not None, "flow is tagged with a process row id")
    if pid_col is None: idx.close(); return
    conn = sqlite3.connect(str(sp)); conn.row_factory = sqlite3.Row
    try:
        prow = conn.execute("SELECT * FROM process WHERE id=?", (pid_col,)).fetchone()
        env_json = conn.execute("SELECT env FROM env WHERE id=?",
                                (prow["env_id"],)).fetchone()
    finally:
        conn.close()
    check(prow["tgid"] == child.pid, "process row keyed by the child's HOST pid")
    check(prow["exe"] == real_exe, "process exe is the child's real host exe")
    check(json.loads(prow["argv"])[0].endswith("cat"), "process argv is the real argv")
    env = json.loads(env_json[0]) if env_json else {}
    check(isinstance(env, dict) and len(env) > 0,
          "process env recorded from the child's host /proc/environ")
    idx.close()


def test_relay_payload_tagging_dedup():
    """C + D: drive log_flow exactly as the relay path does (fl['prov'] = the host
    provenance dict). Asserts process_from_prov inserts/dedups from it and that two
    flows from one HOST tgid share one process row (S4: both written, no silent drop)."""
    # The box root is this process's real host parent, so the flow process's PPid
    # chain bubbles up exactly to it (a genuine ancestor) and stops — no walk into
    # unrelated host system processes.
    sid = "4102"
    sup, idx, sp = _mk_supervisor(sid, tracing=True)
    # 3c records the box root row at register; emulate it.
    idx.process_from_prov(dict(tgid=m.tgid_of(os.getppid()), ppid=0,
                               exe="", argv=["root"], env={}))

    # Build a real host provenance dict for THIS test process via its own pidfd, so the
    # tgid is genuinely a live host pid (no fabricated values).
    pidfd = os.pidfd_open(os.getpid(), 0)
    prov = _resolve_prov_via_pidfd(pidfd)
    os.close(pidfd)
    check(prov["tgid"] == os.getpid(), "prov tgid is this process's host pid")

    sup.log_flow(sid, dict(ts=1.0, action="allow", method="GET", scheme="http",
                           host="x", port=80, url="http://x/", status=200,
                           prov=dict(prov), req_headers=[], req_body=b"",
                           resp_headers=[], resp_body=b"ok"))
    sup.log_flow(sid, dict(ts=2.0, action="allow", method="GET", scheme="http",
                           host="x", port=80, url="http://x/2", status=204,
                           prov=dict(prov), req_headers=[], req_body=b"",
                           resp_headers=[], resp_body=b""))

    flows = m.flows_list(sp)
    check(len(flows) == 2, "both flows written via the single Index writer (S4: no drop)")
    pids = {f["process_id"] for f in flows}
    check(len(pids) == 1 and None not in pids,
          "both flows from one host tgid share one deduped process row")

    # The flow's tgid is this process; its PPid chain bubbled up to the box root, so
    # the table is one connected tree. Both flows still share the SINGLE row for this
    # tgid (dedup), and that row carries the host tgid.
    conn = sqlite3.connect(str(sp))
    try:
        nself = conn.execute("SELECT COUNT(*) FROM process WHERE tgid=?",
                             (os.getpid(),)).fetchone()[0]
        row = conn.execute("SELECT tgid FROM process WHERE id=?",
                           (list(pids)[0],)).fetchone()
    finally:
        conn.close()
    check(nself == 1, "exactly one process row for the one host tgid")
    check(row[0] == os.getpid(), "row carries the host tgid")
    env = m.process_env(sp, list(pids)[0])
    check(isinstance(env, dict) and len(env) > 0, "host env recorded under the process row")
    idx.close()


def test_peer_gone_and_no_pidfd_leave_untagged():
    """E: a pidfd of an already-exited process resolves to host pid <= 0, and the
    no-pidfd case yields {"tgid":0}; both leave the flow recorded but UNTAGGED -- no
    bogus process row is invented from a missing peer."""
    sid = "4103"
    sup, idx, sp = _mk_supervisor(sid, tracing=True)

    # peer-gone: open a pidfd, then let the child exit + be reaped, then resolve. A
    # dead pidfd's fdinfo reports Pid: 0 / -1 -> tgid 0 -> untagged.
    child, child_w = _spawn_child()
    pidfd = os.pidfd_open(child.pid, 0)
    os.close(child_w); child.terminate(); child.wait()
    # give the kernel a moment to mark the pidfd's target dead in fdinfo
    deadline = time.time() + 5
    while time.time() < deadline and m._host_pid_from_pidfd(pidfd) > 0:
        time.sleep(0.05)
    prov_gone = _resolve_prov_via_pidfd(pidfd)
    os.close(pidfd)
    check(prov_gone["tgid"] == 0, "pidfd of a reaped process resolves to no host pid")

    sup.log_flow(sid, dict(ts=1.0, action="allow", method="GET", scheme="http",
                           host="y", port=80, url="http://y/", status=200,
                           prov=dict(prov_gone), req_headers=[], req_body=b"",
                           resp_headers=[], resp_body=b""))
    # no-pidfd case: relay sets {"tgid":0} when no pidfd ancillary fd is present.
    sup.log_flow(sid, dict(ts=2.0, action="allow", method="GET", scheme="http",
                           host="y", port=80, url="http://y/2", status=200,
                           prov={"tgid": 0}, req_headers=[], req_body=b"",
                           resp_headers=[], resp_body=b""))

    flows = m.flows_list(sp)
    check(len(flows) == 2, "both flows written (S4: no drop)")
    check(all(f["process_id"] is None for f in flows),
          "flows with no resolvable peer are written but left untagged")
    conn = sqlite3.connect(str(sp))
    try: nproc = conn.execute("SELECT COUNT(*) FROM process").fetchone()[0]
    finally: conn.close()
    check(nproc == 0, "no process row invented from a missing/gone peer")
    idx.close()


def main():
    try:
        test_live_http_flow_through_engine()
        test_relay_payload_tagging_dedup()
        test_peer_gone_and_no_pidfd_leave_untagged()
    finally:
        shutil.rmtree(_TMP, ignore_errors=True)
    if _fails:
        print(f"\n{len(_fails)} FAILED"); return 1
    print("\nNET-FLOW PASS"); return 0


if __name__ == "__main__":
    sys.exit(main())
