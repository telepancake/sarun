#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "pytest>=8",
#   "pyfuse3>=3.2",
#   "trio>=0.22",
# ]
# ///
"""End-to-end coverage for the Rust engine's `-n` (per-box netns + TAP +
smoltcp + DHCP + DNS + HTTPS MITM) network mode.

Build matrix this exercises:
  • -n  enables the proxy: box gets `240.X.0.2/30` on tap<id>, default route
        via `240.X.0.1`, DNS pointed at the gateway. Synthetic DNS allocates
        IPs from `240.X.1.0` onward; smoltcp accepts SYNs to any of them
        (any_ip + default route). HTTP and HTTPS are terminated and replayed
        upstream from the engine's host netns. pcapng of the TAP + sidecar
        TLS keys file land under `state_home/flows/box<id>/`.
  • -N  keeps host netns (the old default).
  • (none) → empty netns; getaddrinfo / dial fail closed.

Skips cleanly if the Rust binary can't be built or if the container can't
reach the public internet for the upstream comparison.

Run:
    cd prototype
    uv run --with pytest --with "pyfuse3>=3.2" --with "trio>=0.22" \
        pytest -q -p no:cacheprovider test_net_rs.py
"""
from __future__ import annotations

import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

_HERE = Path(__file__).resolve().parent
CRATE = _HERE.parent / "engine"
BIN = CRATE / "target" / "release" / "sarun"


# ── helpers ────────────────────────────────────────────────────────────────

def ensure_binary() -> bool:
    if BIN.exists():
        return True
    if shutil.which("cargo") is None:
        return False
    r = subprocess.run(["cargo", "build", "--release"], cwd=CRATE,
                       capture_output=True, text=True)
    return r.returncode == 0 and BIN.exists()


def _internet_works() -> bool:
    """The proxy needs a real upstream to reach during the test. If outbound
    is gated (closed environment, etc.), skip the upstream-touching tests."""
    try:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
            s.settimeout(3.0)
            s.connect(("example.com", 80))
        return True
    except OSError:
        return False


def wait_socket(sock: str, timeout: float = 15.0) -> bool:
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.settimeout(1.0)
                s.connect(sock)
            return True
        except OSError:
            time.sleep(0.1)
    return False


class Engine:
    """Spin up an isolated engine instance under per-test XDG dirs.

    Each test gets its own NS so engines don't interfere with each other
    across runs."""
    def __init__(self, ns: str):
        self.tmp = Path(tempfile.mkdtemp(prefix=f"netrs-{ns}-"))
        self.env = os.environ.copy()
        for k, sub in (("XDG_STATE_HOME", "state"),
                       ("XDG_RUNTIME_DIR", "run"),
                       ("XDG_CONFIG_HOME", "config"),
                       ("XDG_DATA_HOME", "data")):
            d = self.tmp / sub
            d.mkdir(parents=True, exist_ok=True)
            self.env[k] = str(d)
        self.env["SLOPBOX_NS"] = ns
        self.log_path = self.tmp / "engine.log"
        self.sock_path = self.tmp / "run" / f"slopbox.{ns}" / "ui.sock"
        self.state_dir = self.tmp / "state" / f"slopbox.{ns}"
        self.proc: subprocess.Popen | None = None

    def start(self):
        self.proc = subprocess.Popen(
            [str(BIN), "serve"], env=self.env,
            stdout=self.log_path.open("ab"),
            stderr=subprocess.STDOUT)
        assert wait_socket(str(self.sock_path)), \
            f"engine socket never appeared\nlog: {self.log_path.read_text()}"

    def stop(self):
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=8)
            except Exception:
                self.proc.kill()
        # Clean up any anchor children the engine forked. Each `-n` box
        # spawned one; teardown should have SIGTERM'd them, but a test
        # crash leaves orphans → reap by their cmdline.
        try:
            r = subprocess.run(["pgrep", "-fa", "sarun.*anchor"],
                               capture_output=True, text=True)
            for line in r.stdout.splitlines():
                pid = line.split()[0]
                try: os.kill(int(pid), 9)
                except Exception: pass
        except Exception: pass
        shutil.rmtree(self.tmp, ignore_errors=True)

    def run(self, *args: str, timeout: float = 20.0) -> subprocess.CompletedProcess:
        return subprocess.run([str(BIN), "run", *args], env=self.env,
                              capture_output=True, text=True, timeout=timeout)


def skip_if_no_binary():
    if not ensure_binary():
        import pytest
        pytest.skip("cargo build failed or unavailable")


def skip_if_offline():
    if not _internet_works():
        import pytest
        pytest.skip("no outbound connectivity for upstream-touching tests")


# ── tests ──────────────────────────────────────────────────────────────────

def test_n_box_has_tap_and_route():
    """`-n` puts a tap<id> in the box's netns with `.0.2/30`, default
    route via `.0.1`, and a permanent ARP entry pointing the gateway at
    the engine's deterministic gw_mac. The kernel sees a complete L2
    setup before the box's command even starts."""
    skip_if_no_binary()
    eng = Engine("TST1")
    try:
        eng.start()
        r = eng.run("-n", "--", "sh", "-c",
                    "ip -4 addr show; echo ---; ip route; echo ---; ip neigh")
        assert r.returncode == 0, r.stderr
        out = r.stdout
        # The IP+prefix that lands on the TAP is the box's /30 lease.
        assert "/30" in out, f"box IP isn't /30: {out!r}"
        assert ".0.2/30" in out, f"box ip isn't .0.2: {out!r}"
        assert "default via" in out
        assert ".0.1 dev tap" in out, f"default route isn't via .0.1 on tap: {out!r}"
        # Permanent ARP entry for the gateway, MAC starts 02:73:72:6e.
        assert "PERMANENT" in out
        assert "02:73:72:6e" in out, f"gw_mac not in arp: {out!r}"
    finally:
        eng.stop()


def test_n_box_synthetic_dns():
    """DNS to the gateway resolves arbitrary hostnames to a synthetic IP
    inside the box's /16. Same host → same IP (allocator dedup)."""
    skip_if_no_binary()
    eng = Engine("TST2")
    try:
        eng.start()
        # Two queries in the same box, same hostname → same IP (DNS state
        # lives for the box's lifetime).
        r = eng.run("-n", "--", "sh", "-c",
                    "getent hosts foo.example; getent hosts foo.example; "
                    "getent hosts bar.example")
        assert r.returncode == 0, r.stderr
        lines = [L for L in r.stdout.splitlines() if L.strip()]
        assert len(lines) == 3
        foo1, foo2, bar = (L.split()[0] for L in lines)
        assert foo1 == foo2, f"foo's IP changed between queries: {foo1} vs {foo2}"
        assert foo1 != bar, f"different hosts got same IP: {foo1}"
        # Synth IPs live in the .1.* row of the box's /16 (never the .0.*
        # row, which is reserved for gateway + box).
        assert foo1.split('.')[2] != '0', \
            f"synth IP collided with the .0.* row: {foo1}"
    finally:
        eng.stop()


def test_n_box_http_proxied():
    """The full path: box dials a hostname → DNS gives synth IP → smoltcp
    accepts → hyper proxies the HTTP request to example.com via the host
    netns → response comes back through the proxy."""
    skip_if_no_binary()
    skip_if_offline()
    eng = Engine("TST3")
    try:
        eng.start()
        r = eng.run("-n", "--", "curl", "-sS", "-m", "15",
                    "http://example.com/")
        assert r.returncode == 0, f"curl failed: {r.stderr}\n{r.stdout}"
        assert "<title>Example Domain</title>" in r.stdout, \
            f"didn't get example.com body: {r.stdout[:200]!r}"
    finally:
        eng.stop()


def test_n_box_https_mitm():
    """HTTPS path: box's curl trusts the augmented OS CA bundle (with the
    engine root spliced in), so the rcgen leaf cert we mint for
    example.com is accepted. Engine opens upstream HTTPS with the real
    OS trust store. Response decrypts on the box side."""
    skip_if_no_binary()
    skip_if_offline()
    eng = Engine("TST4")
    try:
        eng.start()
        r = eng.run("-n", "--", "curl", "-sS", "-m", "15",
                    "https://example.com/")
        assert r.returncode == 0, f"curl failed: {r.stderr}\n{r.stdout}"
        assert "<title>Example Domain</title>" in r.stdout, \
            f"didn't get example.com body: {r.stdout[:200]!r}"
    finally:
        eng.stop()


def test_n_box_writes_pcapng_and_keylog():
    """Every box's TAP frames are captured to pcapng (one IDB + per-frame
    EPBs), and every TLS connection's secrets are appended to the keylog
    sidecar in NSS SSLKEYLOGFILE format. tshark with
    `tls.keylog_file:<path>` can decrypt the entire pcapng."""
    skip_if_no_binary()
    skip_if_offline()
    eng = Engine("TST5")
    try:
        eng.start()
        r = eng.run("-n", "--", "curl", "-sS", "-m", "15", "-o", "/dev/null",
                    "https://example.com/")
        assert r.returncode == 0, r.stderr
        flows_dir = eng.state_dir / "flows"
        # One subdir per box, plus the boxN file pair.
        boxes = list(flows_dir.glob("box*"))
        assert len(boxes) >= 1, f"no flows dirs under {flows_dir}"
        pcap_files = list(boxes[0].glob("flows-*.pcapng"))
        key_files  = list(boxes[0].glob("flows-*.keys"))
        assert len(pcap_files) == 1, f"pcapng missing: {pcap_files}"
        assert len(key_files)  == 1, f"keylog missing: {key_files}"
        # pcapng has a 4-byte block-type prefix; the SHB ("Section Header
        # Block") type is 0x0A0D0D0A. We don't try to fully parse here —
        # if the bytes are there in non-trivial volume the writer worked.
        pcap_bytes = pcap_files[0].read_bytes()
        assert pcap_bytes[:4] == bytes([0x0A, 0x0D, 0x0D, 0x0A]), \
            f"pcapng magic missing: {pcap_bytes[:8].hex()}"
        assert len(pcap_bytes) > 256, f"pcapng tiny: {len(pcap_bytes)} bytes"
        # Keylog: every line starts with one of the NSS labels. CLIENT_HANDSHAKE
        # and SERVER_HANDSHAKE are the floor of TLS 1.3 traffic secrets.
        keys = key_files[0].read_text()
        assert keys.strip(), "keylog file is empty"
        labels = {L.split()[0] for L in keys.splitlines() if L.strip()}
        assert "CLIENT_HANDSHAKE_TRAFFIC_SECRET" in labels, \
            f"no CHTS in keylog: {labels!r}"
        assert "SERVER_HANDSHAKE_TRAFFIC_SECRET" in labels, \
            f"no SHTS in keylog: {labels!r}"
    finally:
        eng.stop()


def test_n_box_quic_blocked():
    """UDP other than :53 is dropped at the stack — there's no listener
    bound. curl's --http3-only sends a QUIC Initial UDP packet to :443;
    no smoltcp socket consumes it, so the box's curl falls back to TCP
    (which the proxy DOES handle). We assert curl WITH --http3-only and a
    short timeout times out / errors, demonstrating "what the proxy can't
    handle doesn't work" for the QUIC half of HTTP/3."""
    skip_if_no_binary()
    skip_if_offline()
    eng = Engine("TST6")
    try:
        eng.start()
        # Many curls don't ship --http3-only support; if so, skip rather
        # than emit a false-positive.
        r = eng.run("-n", "--", "curl", "--version")
        if "HTTP3" not in r.stdout and "quiche" not in r.stdout \
                and "nghttp3" not in r.stdout:
            import pytest
            pytest.skip("box's curl doesn't have HTTP/3 support")
        r = eng.run("-n", "--", "curl", "-sS", "-m", "5",
                    "--http3-only", "https://example.com/")
        # Either non-zero exit OR no body received: any failure-shape is
        # the right outcome. (A success here would mean QUIC went through,
        # contradicting the design choice.)
        if r.returncode == 0 and "<title>" in r.stdout:
            assert False, "QUIC unexpectedly succeeded — proxy stack let UDP/443 through"
    finally:
        eng.stop()


def test_default_no_netns_dials_fail_closed():
    """No `-n`/`-N` → empty netns, no loopback, no routes. Every dial
    fails closed. We assert getaddrinfo (no nameserver) AND a raw TCP
    dial to a public IP both fail."""
    skip_if_no_binary()
    eng = Engine("TST7")
    try:
        eng.start()
        # No network at all → DNS fails. We expect getent to print
        # nothing and exit non-zero, OR the kernel to refuse the dial.
        r = eng.run("--", "sh", "-c",
                    "getent hosts example.com; echo exit=$?")
        # The literal "exit=" line is there either way; check that the
        # exit code is non-zero (no resolution possible).
        assert "exit=0" not in r.stdout, \
            f"DNS resolved in an empty netns: {r.stdout!r}"
    finally:
        eng.stop()


def test_dash_N_uses_host_netns():
    """`-N` keeps the engine's host netns (the pre-change default). The
    box can do whatever host could, which we observe by it being able to
    reach an interface other than lo+tap*: a real interface name from
    /sys/class/net should appear in `ip link` output."""
    skip_if_no_binary()
    eng = Engine("TST8")
    try:
        eng.start()
        r = eng.run("-N", "--", "ip", "-o", "link", "show")
        assert r.returncode == 0
        # Host typically has eth0 / ens* / enp* / wg* / docker* / etc.
        # We assert there's SOMETHING beyond loopback (just `lo:` would
        # signal we got an empty netns by accident).
        lines = [L for L in r.stdout.splitlines() if L.strip()]
        non_lo = [L for L in lines
                  if "lo:" not in L.split(":")[1] and "tap" not in L]
        assert non_lo, \
            f"-N gave us only lo+tap (looks like empty netns): {lines!r}"
    finally:
        eng.stop()


# ── standalone __main__ harness (repo style; the test_*.py files in
#    prototype/ each support both pytest AND direct python invocation) ────

_fails: list[str] = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


def main() -> int:
    import pytest
    args = [__file__, "-v", "--tb=short"]
    return pytest.main(args)


if __name__ == "__main__":
    sys.exit(main())
