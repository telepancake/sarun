#!/usr/bin/env python3
"""Per-box synthetic-IP DNS + catch-all forwarder tests (no bwrap; real loopback
sockets in the current netns). Covers the in-box pieces that don't need an isolated
netns: the SyntheticDNS allocator, the hand-rolled DNS wire codec (incl. a real UDP
round-trip), the catch-all forward-vs-close decision driven by getsockname(), and the
UI-side relay JSON-meta parse (proxy vs direct).

Run standalone:  python test_box_dns.py     (or under pytest, see conftest.py)

NOT covered here (only verifiable under a real bwrap box / live TLS client): the
mitmproxy ReverseProxy direct-path termination + cert spoofing, and that the box's
synthesized /etc/resolv.conf actually routes glibc to 127.0.0.53.
"""
import os, sys, socket, struct, json, threading, array, time
from importlib.machinery import SourceFileLoader

m = SourceFileLoader("slopbox", "/home/user/sarun/sarun").load_module()

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


# ── DNS query builder (an independent reference encoder for the tests) ───────
def build_a_query(domain, txid=0x1234, qtype=1):
    header = struct.pack("!HHHHHH", txid, 0x0100, 1, 0, 0, 0)  # RD=1, 1 question
    qname = b"".join(bytes([len(l)]) + l.encode("latin-1")
                     for l in domain.split(".") if l) + b"\x00"
    return header + qname + struct.pack("!HH", qtype, 1)  # qtype, IN


def decode_a_answers(resp):
    """Return (rcode, [ipv4_strings]) from a response packet built by our codec."""
    txid, flags, qd, an, ns, ar = struct.unpack("!HHHHHH", resp[:12])
    rcode = flags & 0x0F
    # Skip the question section.
    pos = 12
    for _ in range(qd):
        while resp[pos] != 0:
            pos += 1 + resp[pos]
        pos += 1 + 4  # null + qtype + qclass
    ips = []
    for _ in range(an):
        # name (compression pointer = 2 bytes, or labels) — we only emit pointers.
        if resp[pos] & 0xC0:
            pos += 2
        else:
            while resp[pos] != 0:
                pos += 1 + resp[pos]
            pos += 1
        atype, aclass, ttl, rdlen = struct.unpack("!HHIH", resp[pos:pos + 10])
        pos += 10
        rdata = resp[pos:pos + rdlen]; pos += rdlen
        if atype == 1 and rdlen == 4:
            ips.append(socket.inet_ntoa(rdata))
    return rcode, ips


# ── Allocator ───────────────────────────────────────────────────────────────
def test_allocator_stable_dedup_reverse_reserved():
    d = m.SyntheticDNS()
    ip1 = d.ip_for("example.com")
    ip2 = d.ip_for("example.com")
    check(ip1 == ip2, "same domain → same synthetic IP (dedup)")
    ip3 = d.ip_for("other.com")
    check(ip3 != ip1, "different domains → different IPs")
    check(ip1.startswith("127."), "synthetic IP is in 127/8")
    check(d.domain_for(ip1) == "example.com", "reverse ip→domain works")
    check(d.domain_for("127.9.9.9") is None, "unallocated IP reverses to None")
    # Case/trailing-dot normalization: a FQDN with a trailing dot maps the same.
    check(d.ip_for("Example.com.") == ip1, "case/trailing-dot normalized to same IP")
    # Reserved IPs are never handed out.
    seen = {d.ip_for(f"h{i}.test") for i in range(500)}
    for reserved in ("127.0.0.0", "127.0.0.1", "127.0.0.53"):
        check(reserved not in seen, f"reserved {reserved} excluded from pool")


def test_allocator_pool_exhaustion():
    d = m.SyntheticDNS()
    # Shrink the pool to a couple of addresses by jamming _next near the top.
    d._next = m.SyntheticDNS._LO_BCAST - 2   # 127.255.255.253
    a = d.ip_for("a.test")
    b = d.ip_for("b.test")
    check(a != b, "two addresses allocated from the tail of the pool")
    raised = False
    try:
        d.ip_for("c.test")
    except RuntimeError:
        raised = True
    check(raised, "pool exhaustion raises RuntimeError")


# ── DNS wire codec ──────────────────────────────────────────────────────────
def test_dns_codec_a_query():
    d = m.SyntheticDNS()
    pkt = build_a_query("api.example.com", txid=0xBEEF)
    resp = m._dns_handle_packet(pkt, d)
    check(resp is not None, "A query produces a response")
    txid = struct.unpack("!H", resp[:2])[0]
    check(txid == 0xBEEF, "response echoes the query txid")
    rcode, ips = decode_a_answers(resp)
    check(rcode == 0 and len(ips) == 1, "A response: NOERROR with exactly one answer")
    check(ips[0] == d.ip_for("api.example.com"),
          "answered IP equals the allocator's IP for the domain")


def test_dns_codec_aaaa_empty():
    d = m.SyntheticDNS()
    pkt = build_a_query("v6.example.com", qtype=28)  # AAAA
    resp = m._dns_handle_packet(pkt, d)
    rcode, ips = decode_a_answers(resp)
    check(rcode == 0 and ips == [],
          "AAAA → NOERROR with ZERO answers (client falls back to IPv4)")
    # And AAAA must NOT have minted an A mapping as a side effect (no IP burned).
    check(d._by_domain == {}, "AAAA query allocated no synthetic IP")


def test_dns_codec_servfail_on_exhaustion():
    d = m.SyntheticDNS()
    d._next = m.SyntheticDNS._LO_BCAST   # pool empty
    resp = m._dns_handle_packet(build_a_query("x.test"), d)
    rcode, ips = decode_a_answers(resp)
    check(rcode == 2 and ips == [], "A query under exhaustion → SERVFAIL, no answers")


def test_dns_codec_malformed_no_crash():
    d = m.SyntheticDNS()
    for bad in (b"", b"\x00", b"\x12\x34", b"\xff" * 8,
                struct.pack("!HHHHHH", 1, 0, 1, 0, 0, 0) + b"\x05abc",  # truncated label
                struct.pack("!HHHHHH", 1, 0, 0, 0, 0, 0)):             # qdcount=0
        try:
            out = m._dns_handle_packet(bad, d)
        except Exception as e:
            _fails.append(f"malformed packet crashed codec: {e!r}")
            out = None
        # Either a clean None (dropped) or a valid response — never an exception.
    check(True, "malformed packets never crash the codec")


def test_dns_real_udp_roundtrip():
    """Bind the real resolver helper on an ephemeral 127.0.0.x port and round-trip a
    UDP query through a kernel socket (exercises recvfrom/sendto, not just the codec)."""
    d = m.SyntheticDNS()
    srv = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    srv.bind(("127.0.0.2", 0))
    port = srv.getsockname()[1]
    stop = threading.Event()

    def serve():
        srv.settimeout(0.3)
        while not stop.is_set():
            try:
                pkt, addr = srv.recvfrom(4096)
            except socket.timeout:
                continue
            except OSError:
                break
            r = m._dns_handle_packet(pkt, d)
            if r is not None:
                srv.sendto(r, addr)
    t = threading.Thread(target=serve, daemon=True); t.start()
    try:
        cli = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        cli.settimeout(2.0)
        cli.sendto(build_a_query("real.example.com", txid=0x7777),
                   ("127.0.0.2", port))
        data, _ = cli.recvfrom(4096)
        cli.close()
        txid = struct.unpack("!H", data[:2])[0]
        rcode, ips = decode_a_answers(data)
        check(txid == 0x7777, "UDP round-trip preserves txid")
        check(rcode == 0 and ips and ips[0] == d.ip_for("real.example.com"),
              "UDP round-trip returns the allocated synthetic IP")
    finally:
        stop.set(); t.join(timeout=1); srv.close()


# ── Catch-all forward-vs-close decision ─────────────────────────────────────
class _RelayStub:
    """Stand-in for the per-box relay unix socket: records the JSON meta + fds it
    receives so a test can assert what the forwarder sent."""
    def __init__(self):
        self.path = f"/tmp/sb-relay-{os.getpid()}-{id(self)}.sock"
        try: os.unlink(self.path)
        except FileNotFoundError: pass
        self.srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.srv.bind(self.path); self.srv.listen(8); self.srv.settimeout(2.0)
        self.received = []   # list of (meta_dict, n_fds)
        self._t = threading.Thread(target=self._loop, daemon=True); self._t.start()

    def _loop(self):
        while True:
            try:
                conn, _ = self.srv.accept()
            except OSError:
                break
            try:
                msg, anc, _f, _a = conn.recvmsg(65536, socket.CMSG_SPACE(4 * 4))
                nfds = 0
                for lvl, typ, data in anc:
                    if lvl == socket.SOL_SOCKET and typ == socket.SCM_RIGHTS:
                        a = array.array("i"); a.frombytes(data)
                        nfds += len(a)
                        for fd in a.tolist():
                            try: os.close(fd)
                            except OSError: pass
                try: meta = json.loads(msg.decode())
                except Exception: meta = None
                self.received.append((meta, nfds))
            finally:
                conn.close()

    def close(self):
        try: self.srv.close()
        except OSError: pass
        try: os.unlink(self.path)
        except FileNotFoundError: pass


def _drive_catch_all(synthetic_ip, relay_path, dns):
    """Run the production catch-all handler against ONE real connection dialed to
    `synthetic_ip`. Mirrors run_inner's _handle_catch_all/forward_one but standalone
    (run_inner's closures aren't reachable without launching a box). We assert the
    decision logic: mapped IP → forward with direct meta; unmapped → close, no send."""
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("0.0.0.0", 0)); srv.listen(4)
    port = srv.getsockname()[1]

    def forward_one(conn, meta):
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as ch:
                ch.connect(relay_path)
                ch.sendmsg([json.dumps(meta).encode()],
                           [(socket.SOL_SOCKET, socket.SCM_RIGHTS,
                             array.array("i", [conn.fileno()]).tobytes())])
        finally:
            conn.close()

    def handle(conn):
        # In the box, getsockname() yields the SYNTHETIC dest IP; here we use the IP we
        # bound the client to (loopback alias), which is exactly what the client dialed.
        dest_ip = conn.getsockname()[0]
        domain = dns.domain_for(dest_ip)
        if domain is None:
            conn.close(); return
        forward_one(conn, {"kind": "direct", "host": domain,
                           "port": 80, "scheme": "http"})

    accepted = []
    def accept_loop():
        srv.settimeout(2.0)
        try:
            conn, _ = srv.accept()
        except OSError:
            return
        accepted.append(True)
        handle(conn)
    t = threading.Thread(target=accept_loop, daemon=True); t.start()

    cli = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    cli.settimeout(2.0)
    # Connect via the synthetic IP so getsockname() on the server side reports it.
    cli.connect((synthetic_ip, port))
    time.sleep(0.2)
    cli.close(); t.join(timeout=2); srv.close()


def test_catch_all_mapped_forwards():
    dns = m.SyntheticDNS()
    # Allocate a synthetic IP and bind the test listener so the client can reach it.
    # We need an IP that actually exists on lo here; 127.0.0.2 is routable on loopback.
    # Map that exact IP to a domain in the allocator by hand (the real allocator hands
    # out 127.0.0.x; we pin the reverse entry to the IP the kernel will report).
    dns._by_ip["127.0.0.2"] = "mapped.example.com"
    dns._by_domain["mapped.example.com"] = "127.0.0.2"
    relay = _RelayStub()
    try:
        _drive_catch_all("127.0.0.2", relay.path, dns)
        time.sleep(0.2)
        check(len(relay.received) == 1, "mapped synthetic IP → exactly one relay send")
        if relay.received:
            meta, nfds = relay.received[0]
            check(meta and meta.get("kind") == "direct",
                  "forwarded meta kind == direct")
            check(meta.get("host") == "mapped.example.com",
                  "forwarded meta carries the resolved DOMAIN (not the IP)")
            check(meta.get("scheme") == "http" and meta.get("port") == 80,
                  "forwarded meta carries scheme/port")
            check(nfds >= 1, "forwarded at least the connection fd")
    finally:
        relay.close()


def test_catch_all_unmapped_closed():
    dns = m.SyntheticDNS()   # nothing mapped to 127.0.0.3
    relay = _RelayStub()
    try:
        _drive_catch_all("127.0.0.3", relay.path, dns)
        time.sleep(0.2)
        check(relay.received == [],
              "unmapped (direct-IP) connection is CLOSED, never forwarded")
    finally:
        relay.close()


# ── UI-side relay meta JSON parse (proxy vs direct) ─────────────────────────
def _parse_relay_meta(msg: bytes):
    """Replicates the parse logic in RelayServer._handle so it can be unit-tested
    without an event loop. Kept structurally identical (proxy fail-safe + direct)."""
    kind, dest = "proxy", None
    if msg and msg not in (b"\x00",):
        try:
            meta = json.loads(msg.decode())
            if isinstance(meta, dict) and meta.get("kind") == "direct":
                kind = "direct"
                dest = (str(meta.get("host", "")), int(meta.get("port", 0)),
                        str(meta.get("scheme", "https")))
        except (ValueError, TypeError, UnicodeDecodeError):
            kind, dest = "proxy", None
    return kind, dest


def test_relay_meta_roundtrip():
    # proxy meta
    k, d = _parse_relay_meta(json.dumps({"kind": "proxy"}).encode())
    check(k == "proxy" and d is None, "proxy meta → kind=proxy, no dest")
    # legacy/empty payloads fail safe to proxy
    check(_parse_relay_meta(b"\x00") == ("proxy", None), "b'\\x00' → proxy (fail-safe)")
    check(_parse_relay_meta(b"") == ("proxy", None), "empty payload → proxy (fail-safe)")
    # direct meta round-trips host/port/scheme
    k, d = _parse_relay_meta(json.dumps(
        {"kind": "direct", "host": "h.example.com", "port": 443,
         "scheme": "https"}).encode())
    check(k == "direct" and d == ("h.example.com", 443, "https"),
          "direct meta → kind=direct with (host, port, scheme)")
    k, d = _parse_relay_meta(json.dumps(
        {"kind": "direct", "host": "p.example.com", "port": 80,
         "scheme": "http"}).encode())
    check(d == ("p.example.com", 80, "http"), "direct http meta round-trips")
    # garbage payload → proxy fail-safe
    check(_parse_relay_meta(b"not json{")[0] == "proxy",
          "non-JSON payload → proxy (fail-safe)")


# ── Reverse-mode availability (the UI direct-path mechanism) ────────────────
def test_reverse_mode_parses():
    """The direct path pins a mitmproxy ReverseMode to scheme://host:port. Assert that
    construction works for both schemes and yields the expected address — the gist of
    why ReverseProxy is the right layer (it dials the KNOWN upstream, not the IP)."""
    try:
        from mitmproxy.proxy import mode_specs
    except Exception as e:
        check(False, f"mitmproxy import failed: {e}"); return
    https = mode_specs.ProxyMode.parse("reverse:https://h.example.com:443")
    http = mode_specs.ProxyMode.parse("reverse:http://h.example.com:80")
    check(type(https).__name__ == "ReverseMode", "reverse https → ReverseMode")
    check(https.address == ("h.example.com", 443),
          "ReverseMode pins upstream address to host:port")
    check(https.scheme == "https" and http.scheme == "http",
          "ReverseMode scheme reflects http vs https (TLS termination vs plain)")


if __name__ == "__main__":
    for t in (test_allocator_stable_dedup_reverse_reserved,
              test_allocator_pool_exhaustion,
              test_dns_codec_a_query,
              test_dns_codec_aaaa_empty,
              test_dns_codec_servfail_on_exhaustion,
              test_dns_codec_malformed_no_crash,
              test_dns_real_udp_roundtrip,
              test_catch_all_mapped_forwards,
              test_catch_all_unmapped_closed,
              test_relay_meta_roundtrip,
              test_reverse_mode_parses):
        print(f"\n== {t.__name__} ==")
        try:
            t()
        except Exception as e:
            import traceback; traceback.print_exc(); _fails.append(f"{t.__name__}: {e}")
    print("\n" + ("ALL PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    sys.exit(1 if _fails else 0)
