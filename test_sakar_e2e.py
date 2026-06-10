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
        return self

    def get_flows_path(self) -> "Path | None":
        """Find the flows file the server wrote."""
        if not self._state_dir.exists():
            return None
        flows = sorted(self._state_dir.glob("flows-*.mitm"))
        return flows[-1] if flows else None

    def run_box(self, cmd: list, extra_env: "dict | None" = None,
                timeout: int = TIMEOUT) -> subprocess.CompletedProcess:
        """Run `sakar -- cmd` against this server."""
        env = dict(self._env_obj)
        if extra_env:
            env.update(extra_env)
        full_cmd = [sys.executable, SAKAR, "--"] + cmd
        return subprocess.run(
            full_cmd, env=env, capture_output=True, timeout=timeout,
            text=True)

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


def test_userns_runner_privileged_ports(ctx: ServerContext):
    """Force the unprivileged-user-namespace runner and prove the in-box forwarders
    can still bind the privileged ports 53/80/443.

    This path is NOT taken when sakar runs with ambient CAP_SYS_ADMIN/CAP_NET_ADMIN
    (the usual root/CI case picks the 'ambient' runner), so without forcing it the
    entire userns branch — including how CAP_NET_BIND_SERVICE reaches the inner — is
    never exercised. The regression it guards: the cap was granted but did not survive
    the inner's execve under a non-root uid mapping, so every privileged bind failed
    with 'sakar(inner): cannot listen :443: Permission denied' and the box had no
    working network. We drive the proxy-UNAWARE path (DNS + catch-all on :53/:80/:443)
    so the privileged binds are actually load-bearing for the fetch to succeed."""
    print("  test: forced-userns runner binds privileged ports ...", flush=True)
    r = ctx.run_box(
        ["curl", "-sS", "--noproxy", "*", "--max-time", "30", "https://example.com/"],
        extra_env={"SAKAR_FORCE_USERNS": "1"}, timeout=60)
    if "cannot create the runner sandbox" in r.stderr:
        print("    SKIP: unprivileged user namespaces unavailable on this host",
              flush=True)
        return
    assert "cannot listen" not in r.stderr and "cannot bind DNS" not in r.stderr, \
        f"userns runner could not bind privileged ports:\n{r.stderr[:400]}"
    assert r.returncode == 0 and (
        "example" in r.stdout.lower() or "<html" in r.stdout.lower()), \
        f"forced-userns fetch failed rc={r.returncode}\nstderr:{r.stderr[:300]}"
    print("    PASS: forced-userns runner reached example.com via DNS+catch-all",
          flush=True)


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
            ("userns_runner_privileged_ports", test_userns_runner_privileged_ports),
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
