#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "mitmproxy>=11",
# ]
# ///
"""
test_sakar_e2e.py — end-to-end tests for sakar.

Requires: outbound network, bwrap+netns (all present in this container).
Run:
    ./test_sakar_e2e.py          # directly
    uv run test_sakar_e2e.py     # via uv
"""
from __future__ import annotations

import json
import os
import signal
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

SAKAR = str(Path(__file__).parent / "sakar")
TIMEOUT = 120  # seconds for each test

_USE_DEFAULT = object()  # sentinel: "use the context's default box identity"


def _pick_unprivileged_ids() -> "tuple | None":
    """A non-root (uid, gid) the harness can drop a box to, or None."""
    import pwd
    for p in pwd.getpwall():
        if 1000 <= p.pw_uid < 60000:
            return (p.pw_uid, p.pw_gid)
    try:
        nb = pwd.getpwnam("nobody")
        if nb.pw_uid != 0:
            return (nb.pw_uid, nb.pw_gid)
    except KeyError:
        pass
    return None

# ── helpers ──────────────────────────────────────────────────────────────────

def _sock_path():
    """Derive sakar's sock_path the same way sakar does (no import needed)."""
    base = os.environ.get("XDG_RUNTIME_DIR")
    if base:
        return str(Path(base) / "sakar" / "sakar.sock")
    base = os.environ.get("XDG_DATA_HOME") or os.path.expanduser("~/.local/share")
    return str(Path(base) / "sakar" / "sakar.sock")

def _wait_for_socket(sp, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.settimeout(1.0)
                s.connect(sp)
                return True
        except OSError:
            time.sleep(0.2)
    return False

def _ping_server(sp) -> bool:
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
            s.settimeout(3.0)
            s.connect(sp)
            s.sendall((json.dumps({"type": "ping"}) + "\n").encode())
            line = s.makefile("rb").readline()
            ack = json.loads(line.decode()) if line.strip() else None
            return bool(ack and ack.get("ok"))
    except Exception:
        return False

class ServerContext:
    """Context manager: starts sakar server, stops it on exit."""
    def __init__(self):
        self.proc = None
        self.sp = _sock_path()
        self.flows_path = None
        # If the test harness is root, run the BOXES as an unprivileged user so sakar
        # takes the user-namespace runner (the path a normal user gets) — root would
        # otherwise always exercise the privileged 'ambient' runner. The server itself
        # stays on the uv interpreter (it needs mitmproxy); the box/inner path is
        # stdlib-only, so a dropped box runs fine under the system python.
        self.box_run_as = None
        self.box_python = None

    def __enter__(self):
        # Override XDG dirs to a temp location so tests are isolated.
        env = dict(os.environ)
        self._tmpdir = tempfile.mkdtemp(prefix="sakar-e2e-")
        env["XDG_DATA_HOME"] = self._tmpdir + "/data"
        env["XDG_CONFIG_HOME"] = self._tmpdir + "/config"
        env["XDG_STATE_HOME"] = self._tmpdir + "/state"
        env["XDG_RUNTIME_DIR"] = self._tmpdir + "/run"

        # Re-derive sock_path with the temp dirs.
        self.sp = str(Path(self._tmpdir) / "run" / "sakar" / "sakar.sock")

        # Pre-create the allow file so curl to example.com is allowed without prompting.
        allow_dir = Path(self._tmpdir) / "config" / "sakar"
        allow_dir.mkdir(parents=True, exist_ok=True)
        (allow_dir / "allow").write_text("example.com\n*.example.com\n")
        # Also write a deny file for the deny test
        (allow_dir / "deny").write_text("denied-host-e2e-test.invalid\n")

        self._env = env
        self._env_obj = env

        self.proc = subprocess.Popen(
            [sys.executable, SAKAR],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )

        # Wait for the server to be ready.
        if not _wait_for_socket(self.sp, timeout=40):
            self.proc.terminate()
            stdout, stderr = self.proc.communicate(timeout=5)
            raise RuntimeError(
                f"sakar server did not start within 40s\n"
                f"stdout: {stdout.decode()[:500]}\n"
                f"stderr: {stderr.decode()[:500]}")

        # Extract the flows path from server stdout.
        # The server prints: "sakar: flows → <path>"
        # We'll derive it ourselves since we can't easily read stdout non-blocking.
        state_dir = Path(self._tmpdir) / "state" / "sakar"
        self._state_dir = state_dir

        # Decide how boxes run. As root, drop them to a normal user (the realistic,
        # previously-broken userns path); as non-root, run them as ourselves.
        if os.getuid() == 0:
            ids = _pick_unprivileged_ids()
            if ids is not None and os.path.exists("/usr/bin/python3"):
                self.box_run_as = ids
                self.box_python = "/usr/bin/python3"
                self.make_box_paths_accessible()
                print(f"  (harness is root → running boxes as unprivileged uid "
                      f"{ids[0]}, exercising the userns runner)", flush=True)
            else:
                print("  (harness is root and cannot drop privileges → boxes run "
                      "as root via the ambient runner)", flush=True)
        return self

    def get_flows_path(self) -> "Path | None":
        """Find the flows file the server wrote."""
        if not self._state_dir.exists():
            return None
        flows = sorted(self._state_dir.glob("flows-*.mitm"))
        return flows[-1] if flows else None

    def run_box(self, cmd: list, extra_env: "dict | None" = None,
                timeout: int = TIMEOUT, run_as: "object" = _USE_DEFAULT,
                python: "object" = _USE_DEFAULT) -> subprocess.CompletedProcess:
        """Run `sakar -- cmd` against this server.

        By default the box runs with this context's box identity (dropped to an
        unprivileged uid when the harness is root — see __enter__). Pass run_as=None to
        force running as the harness's own uid (used to exercise the ambient runner as
        root); pass an explicit (uid, gid) to drop to a specific user.
        python: interpreter for the box; the box/inner path is stdlib-only, so the
        world-readable system python is used for dropped boxes (the uv interpreter
        lives in root's cache and a non-root box can't exec it)."""
        if run_as is _USE_DEFAULT:
            run_as = self.box_run_as
        if python is _USE_DEFAULT:
            python = self.box_python
        env = dict(self._env_obj)
        if extra_env:
            env.update(extra_env)
        preexec = None
        if run_as is not None:
            uid, gid = run_as
            env["HOME"] = "/tmp"
            def preexec():
                os.setgid(gid)
                try:
                    import pwd
                    os.initgroups(pwd.getpwuid(uid).pw_name, gid)
                except (KeyError, OSError):
                    pass
                os.setuid(uid)
        full_cmd = [python or sys.executable, SAKAR, "--"] + cmd
        return subprocess.run(
            full_cmd, env=env, capture_output=True, timeout=timeout,
            text=True, preexec_fn=preexec)

    def make_box_paths_accessible(self) -> None:
        """Loosen perms so a dropped (non-root) box launcher can reach the control
        socket and CA cert. Test-only: the real server keeps the socket at 0600."""
        root = Path(self._tmpdir)
        for p in [root, *root.rglob("*")]:
            try:
                os.chmod(p, 0o755 if p.is_dir() else 0o644)
            except OSError:
                pass
        try:
            os.chmod(self.sp, 0o666)
        except OSError:
            pass


    def __exit__(self, *_):
        if self.proc:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        # Cleanup tmpdir
        import shutil
        try:
            shutil.rmtree(self._tmpdir, ignore_errors=True)
        except Exception:
            pass


# ════════════════════════════════════════════════════════════════════════════
#  Tests
# ════════════════════════════════════════════════════════════════════════════

def test_proxy_aware_curl(ctx: ServerContext):
    """curl through the HTTP proxy to example.com — proxy-AWARE path."""
    print("  test: proxy-aware curl to example.com via HTTP_PROXY ...", flush=True)
    r = ctx.run_box(
        ["curl", "-sS", "--max-time", "30", "https://example.com/"],
        timeout=60)
    if r.returncode != 0:
        print(f"    curl stdout: {r.stdout[:500]}", flush=True)
        print(f"    curl stderr: {r.stderr[:500]}", flush=True)
    assert r.returncode == 0, f"curl exited {r.returncode}\nstderr:{r.stderr[:300]}"
    html = r.stdout
    assert "example" in html.lower() or "<html" in html.lower(), \
        f"Expected HTML, got: {html[:200]}"
    print("    PASS: proxy-aware curl succeeded", flush=True)


def test_proxy_unaware_curl(ctx: ServerContext):
    """curl with --noproxy so it uses DNS+catch-all — proxy-UNAWARE path."""
    print("  test: proxy-unaware curl (DNS+catch-all) ...", flush=True)
    # With --noproxy '*' curl won't use HTTP_PROXY but will still use /etc/resolv.conf,
    # which points to our synthetic DNS. The catch-all forwarder intercepts it.
    r = ctx.run_box(
        ["curl", "-sS", "--noproxy", "*", "--max-time", "30",
         "https://example.com/"],
        timeout=60)
    if r.returncode != 0:
        print(f"    curl stdout: {r.stdout[:500]}", flush=True)
        print(f"    curl stderr: {r.stderr[:500]}", flush=True)
    assert r.returncode == 0, \
        f"proxy-unaware curl failed with {r.returncode}\nstderr:{r.stderr[:300]}"
    html = r.stdout
    assert "example" in html.lower() or "<html" in html.lower(), \
        f"Expected HTML, got: {html[:200]}"
    print("    PASS: proxy-unaware curl succeeded", flush=True)


def test_os_truststore_client(ctx: ServerContext):
    """A client that ignores the CA-bundle env vars and reads ONLY the OS trust store
    must still trust the proxy — proving the augmented OS bundle is bound into the box,
    not just advertised via env vars. We strip the CA env vars so curl falls back to
    /etc/ssl/certs."""
    print("  test: OS-trust-store client (CA env vars stripped) ...", flush=True)
    r = ctx.run_box(
        ["env", "-u", "SSL_CERT_FILE", "-u", "SSL_CERT_DIR", "-u", "CURL_CA_BUNDLE",
         "-u", "REQUESTS_CA_BUNDLE", "-u", "GIT_SSL_CAINFO",
         "curl", "-sS", "--noproxy", "*", "--max-time", "30", "https://example.com/"],
        timeout=60)
    # A cert-verification failure (curl exit 60, "certificate" in stderr) would mean the
    # OS store was NOT augmented. Any HTTP response — even a transient upstream 502 from
    # the environment's egress — means the TLS handshake to the proxy succeeded, i.e. the
    # OS store trusts our CA. So we assert "no cert error", not a specific body.
    if r.returncode != 0:
        print(f"    curl stderr: {r.stderr[:500]}", flush=True)
    assert r.returncode == 0 and "certificate" not in r.stderr.lower(), (
        f"OS-trust-store client failed cert verification (rc={r.returncode}); the "
        f"augmented OS bundle is not trusted in-box.\nstderr: {r.stderr[:300]}")
    print("    PASS: OS-trust-store client trusts the proxy CA", flush=True)


def test_denied_host(ctx: ServerContext):
    """curl to a denied host should get a 403 or connection failure."""
    print("  test: denied host returns 403 / fails ...", flush=True)
    r = ctx.run_box(
        ["curl", "-si", "--max-time", "10",
         "http://denied-host-e2e-test.invalid/"],
        timeout=30)
    # curl might get a 403 (status embedded in response) or fail (no route, NXDOMAIN etc.)
    # On deny the server returns a 403; check for 403 in output or non-zero exit.
    output = r.stdout + r.stderr
    got_403 = "403" in output
    # If the host resolves to a synthetic IP the server closes it → curl fails
    # If the deny rule fires the server returns 403 → curl exits with code 22 or 0 w/ body
    ok = got_403 or r.returncode != 0
    assert ok, (
        f"Expected 403 or failure for denied host, got rc={r.returncode}\n"
        f"stdout: {r.stdout[:300]}\nstderr: {r.stderr[:300]}")
    print(f"    PASS: denied host blocked (rc={r.returncode}, 403={got_403})", flush=True)


def test_flow_file(ctx: ServerContext):
    """Flow file exists, is non-empty, and is reloadable via mitmproxy.io.FlowReader."""
    print("  test: flow file exists and is readable ...", flush=True)
    # Wait a moment for the server to flush the flow file.
    time.sleep(1)
    flows_path = ctx.get_flows_path()
    assert flows_path is not None, \
        f"No flows-*.mitm file found in {ctx._state_dir}"
    assert flows_path.exists(), f"Flows file does not exist: {flows_path}"
    size = flows_path.stat().st_size
    assert size > 0, f"Flows file is empty: {flows_path}"
    print(f"    flows file: {flows_path} ({size} bytes)", flush=True)

    # Try to reload with mitmproxy.io.FlowReader.
    try:
        from mitmproxy.io import FlowReader
        with open(flows_path, "rb") as f:
            reader = FlowReader(f)
            flows = list(reader.stream())
        assert len(flows) > 0, "FlowReader returned zero flows"
        hosts = [getattr(getattr(fl, "request", None), "host", None) for fl in flows]
        hosts = [h for h in hosts if h]
        print(f"    flows: {len(flows)} total, hosts: {set(hosts)}", flush=True)
        assert any("example" in (h or "") for h in hosts), \
            f"Expected at least one flow to example.com; got hosts: {hosts}"
        print("    PASS: flow file reloadable with FlowReader", flush=True)
    except ImportError:
        print("    SKIP: mitmproxy.io not available; checking raw bytes instead",
              flush=True)
        # Fallback: just check the file starts with mitmproxy magic
        data = flows_path.read_bytes()
        assert len(data) > 0, "Flows file is empty"
        print("    PASS (fallback): flows file non-empty", flush=True)


def test_userns_runner_keeps_uid(ctx: ServerContext):
    """The box must keep the caller's real uid (never silently become root), while the
    in-box forwarders still bind the privileged ports 53/80/443.

    When the harness is root the context already runs boxes as an unprivileged user
    (the userns runner); when it's non-root every box is the userns runner anyway. Here
    we just pin the invariant: drive the proxy-UNAWARE path (DNS + catch-all on the
    privileged ports, so the binds are load-bearing) and assert the box reports a
    non-root uid. Guards both the original 'cannot listen :443: Permission denied'
    breakage and the uid-0 'fix' that made boxes run as root."""
    print("  test: userns box keeps the caller's uid (binds 53/80/443, not root) ...",
          flush=True)
    if ctx.box_run_as is None and os.getuid() == 0:
        print("    SKIP: harness is root and could not drop to an unprivileged user",
              flush=True)
        return
    expect_uid = ctx.box_run_as[0] if ctx.box_run_as else os.getuid()
    r = ctx.run_box(
        ["bash", "-lc",
         "echo BOXUID=$(id -u); curl -sS --noproxy '*' --max-time 30 "
         "-o /dev/null -w 'HTTP=%{http_code}\\n' http://example.com/ || echo CURLFAIL"],
        timeout=90)
    if "cannot create the runner sandbox" in r.stderr:
        print("    SKIP: unprivileged user namespaces unavailable on this host",
              flush=True)
        return
    for bad in ("cannot listen", "cannot bind DNS",
                "cannot set up box network namespace"):
        assert bad not in r.stderr, \
            f"userns runner failed to set up box networking ({bad!r}):\n{r.stderr[:500]}"
    assert "BOXUID=0" not in r.stdout, \
        f"box ran as root — it must keep the caller's uid:\n{r.stdout[:300]}"
    assert f"BOXUID={expect_uid}" in r.stdout, (
        f"box did not run as the caller's uid {expect_uid}:\n{r.stdout[:300]}")
    assert "HTTP=200" in r.stdout, (
        f"proxy-unaware fetch through the userns box failed:\n"
        f"stdout: {r.stdout[:300]}\nstderr: {r.stderr[:300]}")
    print(f"    PASS: userns box bound :53/:80/:443 as uid {expect_uid} (not root)",
          flush=True)


def test_ambient_runner_as_root(ctx: ServerContext):
    """The privileged 'ambient' runner: only reachable when we actually have host caps,
    so run a box WITHOUT dropping privileges (run_as=None). Skipped unless the harness
    is root, since a normal user can't exercise this path."""
    print("  test: ambient runner (root, no privilege drop) ...", flush=True)
    if os.getuid() != 0:
        print("    SKIP: not root; ambient runner needs host CAP_SYS_ADMIN/NET_ADMIN",
              flush=True)
        return
    r = ctx.run_box(
        ["bash", "-lc",
         "echo BOXUID=$(id -u); curl -sS --noproxy '*' --max-time 30 "
         "-o /dev/null -w 'HTTP=%{http_code}\\n' http://example.com/ || echo CURLFAIL"],
        run_as=None, python=sys.executable, timeout=90)
    for bad in ("cannot listen", "cannot bind DNS"):
        assert bad not in r.stderr, \
            f"ambient runner failed to bind privileged ports ({bad!r}):\n{r.stderr[:500]}"
    assert "BOXUID=0" in r.stdout, \
        f"ambient runner box expected to run as root:\n{r.stdout[:300]}"
    assert "HTTP=200" in r.stdout, (
        f"proxy-unaware fetch through the ambient box failed:\n"
        f"stdout: {r.stdout[:300]}\nstderr: {r.stderr[:300]}")
    print("    PASS: ambient runner bound :53/:80/:443 as root", flush=True)


# ════════════════════════════════════════════════════════════════════════════
#  Main
# ════════════════════════════════════════════════════════════════════════════

def run_all() -> bool:
    print("=== SAKAR E2E TESTS ===", flush=True)
    print("Starting sakar server ...", flush=True)

    failures = []

    with ServerContext() as ctx:
        print(f"Server ready at {ctx.sp}", flush=True)

        tests = [
            ("proxy_aware_curl", test_proxy_aware_curl),
            ("proxy_unaware_curl", test_proxy_unaware_curl),
            ("os_truststore_client", test_os_truststore_client),
            ("denied_host", test_denied_host),
            ("userns_runner_keeps_uid", test_userns_runner_keeps_uid),
            ("ambient_runner_as_root", test_ambient_runner_as_root),
        ]

        for name, fn in tests:
            try:
                fn(ctx)
            except Exception as e:
                import traceback
                print(f"  FAIL [{name}]: {e}", flush=True)
                traceback.print_exc()
                failures.append(name)

        # Flow file test needs to run after the curl tests have produced flows.
        try:
            test_flow_file(ctx)
        except Exception as e:
            import traceback
            print(f"  FAIL [flow_file]: {e}", flush=True)
            traceback.print_exc()
            failures.append("flow_file")

    if failures:
        print(f"\n=== SAKAR E2E FAIL (failed: {', '.join(failures)}) ===",
              flush=True)
        return False
    else:
        print("\nSAKAR E2E PASS", flush=True)
        return True


if __name__ == "__main__":
    ok = run_all()
    sys.exit(0 if ok else 1)
