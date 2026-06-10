"""
test_sakar.py — unit tests for sakar (no FUSE, no pyfuse3, no network required).

Dependencies (pytest-compatible + standalone):
    uv run --with pytest pytest -q -p no:cacheprovider test_sakar.py

Or standalone:
    uv run --with mitmproxy python test_sakar.py
"""

import importlib.machinery
import importlib.util
import json
import os
import socket
import struct
import sys
import tempfile
import textwrap
from pathlib import Path

# ── load sakar without triggering any FUSE/pyfuse3 side-effects ─────────────
def _load_sakar():
    src_path = Path(__file__).parent / "sakar"
    spec = importlib.util.spec_from_loader(
        "sakar",
        importlib.machinery.SourceFileLoader("sakar", str(src_path)))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod

_sakar = _load_sakar()


# ════════════════════════════════════════════════════════════════════════════
#  DNS codec round-trips
# ════════════════════════════════════════════════════════════════════════════

def _make_a_query(txid: int, qname: str) -> bytes:
    """Build a minimal DNS A-query for qname."""
    labels = b""
    for part in qname.split("."):
        enc = part.encode("latin-1")
        labels += bytes([len(enc)]) + enc
    labels += b"\x00"
    # flags: RD=1
    header = struct.pack("!HHHHHH", txid, 0x0100, 1, 0, 0, 0)
    qsection = labels + struct.pack("!HH", _sakar._DNS_QTYPE_A, 1)
    return header + qsection

def _make_aaaa_query(txid: int, qname: str) -> bytes:
    labels = b""
    for part in qname.split("."):
        enc = part.encode("latin-1")
        labels += bytes([len(enc)]) + enc
    labels += b"\x00"
    header = struct.pack("!HHHHHH", txid, 0x0100, 1, 0, 0, 0)
    qsection = labels + struct.pack("!HH", _sakar._DNS_QTYPE_AAAA, 1)
    return header + qsection


def test_dns_a_query_returns_127_ip():
    dns = _sakar.SyntheticDNS()
    pkt = _make_a_query(0xABCD, "example.com")
    resp = _sakar._dns_handle_packet(pkt, dns)
    assert resp is not None, "A query must get a response"
    # Parse out the answer IP: response header (12 bytes) + question + answer rdata.
    # Flags: QR=1, AA=1 → high byte of flags word should have 0x84
    txid, flags, qdcount, ancount = struct.unpack("!HHHH", resp[:8])
    assert txid == 0xABCD
    assert ancount == 1, "Should have one A record answer"
    # The answer IP is the last 4 bytes of the response.
    ip_bytes = resp[-4:]
    assert ip_bytes[0] == 127, f"Should be a 127.x.x.x IP, got {ip_bytes}"
    # Same domain → same IP (stable allocation).
    resp2 = _sakar._dns_handle_packet(pkt, dns)
    assert resp2 is not None
    assert resp2[-4:] == ip_bytes, "Same domain should yield same IP"


def test_dns_aaaa_returns_noerror_empty():
    dns = _sakar.SyntheticDNS()
    pkt = _make_aaaa_query(0x1234, "example.com")
    resp = _sakar._dns_handle_packet(pkt, dns)
    assert resp is not None
    txid, flags, qdcount, ancount = struct.unpack("!HHHH", resp[:8])
    assert txid == 0x1234
    assert ancount == 0, "AAAA should return empty (zero answers)"
    rcode = flags & 0x000F
    assert rcode == 0, "AAAA should return NOERROR"


def test_dns_malformed_returns_none():
    dns = _sakar.SyntheticDNS()
    # Too short to be a valid DNS packet.
    assert _sakar._dns_handle_packet(b"\x00\x01", dns) is None
    # Completely garbage bytes.
    assert _sakar._dns_handle_packet(b"\xff" * 20, dns) is None
    # Empty.
    assert _sakar._dns_handle_packet(b"", dns) is None


def test_dns_other_qtype_returns_empty_noerror():
    """Any qtype other than A or AAAA → NOERROR with zero answers."""
    dns = _sakar.SyntheticDNS()
    # Build a query with qtype=MX (15)
    pkt = _make_a_query(0x5555, "example.com")
    # Patch qtype to MX
    pkt = bytearray(pkt)
    # Find qtype: after the question labels (0-terminated) + 2 bytes before qclass
    pos = 12
    while pkt[pos] != 0:
        pos += 1 + pkt[pos]
    pos += 1  # null label
    struct.pack_into("!H", pkt, pos, 15)  # MX
    resp = _sakar._dns_handle_packet(bytes(pkt), dns)
    assert resp is not None
    ancount = struct.unpack("!H", resp[6:8])[0]
    assert ancount == 0


# ════════════════════════════════════════════════════════════════════════════
#  SyntheticDNS allocation
# ════════════════════════════════════════════════════════════════════════════

def test_synthetic_dns_stable():
    dns = _sakar.SyntheticDNS()
    ip1 = dns.ip_for("example.com")
    ip2 = dns.ip_for("example.com")
    assert ip1 == ip2, "Same domain must always yield same IP"
    assert ip1.startswith("127."), "Should be a 127/8 address"


def test_synthetic_dns_distinct_domains():
    dns = _sakar.SyntheticDNS()
    ip_a = dns.ip_for("alpha.example.com")
    ip_b = dns.ip_for("beta.example.com")
    assert ip_a != ip_b, "Different domains must get different IPs"


def test_synthetic_dns_reverse():
    dns = _sakar.SyntheticDNS()
    ip = dns.ip_for("reverse.example.com")
    assert dns.domain_for(ip) == "reverse.example.com"


def test_synthetic_dns_reverse_unknown():
    dns = _sakar.SyntheticDNS()
    assert dns.domain_for("192.168.1.1") is None


def test_synthetic_dns_reserved_ips_skipped():
    """127.0.0.0, 127.0.0.1, 127.0.0.53 must never be allocated."""
    dns = _sakar.SyntheticDNS()
    allocated = set()
    for i in range(300):
        ip = dns.ip_for(f"host{i}.test")
        allocated.add(ip)
    for forbidden in ("127.0.0.0", "127.0.0.1", "127.0.0.53"):
        assert forbidden not in allocated, f"{forbidden} must not be allocated"


def test_synthetic_dns_case_insensitive():
    dns = _sakar.SyntheticDNS()
    ip1 = dns.ip_for("Example.COM")
    ip2 = dns.ip_for("example.com")
    assert ip1 == ip2, "Domain allocation must be case-insensitive"


# ════════════════════════════════════════════════════════════════════════════
#  Policy
# ════════════════════════════════════════════════════════════════════════════

def test_policy_deny_over_allow(tmp_path):
    """Deny takes precedence over allow."""
    deny = tmp_path / "deny"
    allow = tmp_path / "allow"
    deny.write_text("*.evil.com\n")
    allow.write_text("*.evil.com\nexample.com\n")

    import unittest.mock as mock
    p = _sakar.Policy()
    with mock.patch.object(_sakar, "deny_file", return_value=deny), \
         mock.patch.object(_sakar, "allow_file", return_value=allow):
        assert p.precheck("bad.evil.com") == "deny"
        assert p.precheck("example.com") == "allow"


def test_policy_wildcard_matching(tmp_path):
    deny = tmp_path / "deny"
    allow = tmp_path / "allow"
    deny.write_text("*.blocked.org\n")
    allow.write_text("*.allowed.net\nspecific.host.com\n")

    import unittest.mock as mock
    p = _sakar.Policy()
    with mock.patch.object(_sakar, "deny_file", return_value=deny), \
         mock.patch.object(_sakar, "allow_file", return_value=allow):
        assert p.precheck("sub.blocked.org") == "deny"
        assert p.precheck("blocked.org") is None   # no wildcard match
        assert p.precheck("sub.allowed.net") == "allow"
        assert p.precheck("specific.host.com") == "allow"
        assert p.precheck("unknown.example.com") is None


def test_policy_non_tty_fails_closed(monkeypatch):
    """Non-TTY stdin → deny without prompting."""
    import asyncio, unittest.mock as mock
    p = _sakar.Policy()
    # Patch deny_file and allow_file to empty files (no rules → would prompt).
    with tempfile.TemporaryDirectory() as td:
        tdp = Path(td)
        empty_deny = tdp / "deny"
        empty_allow = tdp / "allow"
        empty_deny.write_text("")
        empty_allow.write_text("")
        with mock.patch.object(_sakar, "deny_file", return_value=empty_deny), \
             mock.patch.object(_sakar, "allow_file", return_value=empty_allow), \
             mock.patch.object(_sakar.sys.stdin, "isatty", return_value=False):
            result = asyncio.run(p.decide("1", "unknown.com", 443, "https"))
    assert result == {"action": "deny"}, "Non-TTY must fail closed to deny"


def test_policy_allow_file_append(tmp_path):
    """'a' choice appends host to allow file."""
    import asyncio, unittest.mock as mock
    deny = tmp_path / "deny"
    allow = tmp_path / "allow"
    deny.write_text("")
    allow.write_text("")

    p = _sakar.Policy()
    # Simulate user typing 'a'
    with mock.patch.object(_sakar, "deny_file", return_value=deny), \
         mock.patch.object(_sakar, "allow_file", return_value=allow), \
         mock.patch.object(_sakar.sys.stdin, "isatty", return_value=True), \
         mock.patch("builtins.input", return_value="a"):
        result = asyncio.run(p.decide("1", "newhost.example.com", 443, "https"))
    assert result == {"action": "allow"}
    content = allow.read_text()
    assert "newhost.example.com" in content


def test_policy_deny_file_append(tmp_path):
    """'d' choice appends host to deny file."""
    import asyncio, unittest.mock as mock
    deny = tmp_path / "deny"
    allow = tmp_path / "allow"
    deny.write_text("")
    allow.write_text("")

    p = _sakar.Policy()
    with mock.patch.object(_sakar, "deny_file", return_value=deny), \
         mock.patch.object(_sakar, "allow_file", return_value=allow), \
         mock.patch.object(_sakar.sys.stdin, "isatty", return_value=True), \
         mock.patch("builtins.input", return_value="d"):
        result = asyncio.run(p.decide("1", "badhost.example.com", 443, "https"))
    assert result == {"action": "deny"}
    content = deny.read_text()
    assert "badhost.example.com" in content


# ════════════════════════════════════════════════════════════════════════════
#  Frame encode/decode
# ════════════════════════════════════════════════════════════════════════════

def test_frame_encode_decode_empty():
    frame = _sakar.encode_frame(_sakar.FRAME_RELAY)
    frames, rem = _sakar.decode_frames(frame)
    assert len(frames) == 1
    assert frames[0] == (_sakar.FRAME_RELAY, b"")
    assert rem == b""


def test_frame_encode_decode_with_payload():
    payload = b'{"kind":"proxy"}'
    frame = _sakar.encode_frame(_sakar.FRAME_RELAY, payload)
    frames, rem = _sakar.decode_frames(frame)
    assert len(frames) == 1
    ftype, fpayload = frames[0]
    assert ftype == _sakar.FRAME_RELAY
    assert fpayload == payload
    assert rem == b""


def test_frame_decode_multiple():
    p1 = b"first"
    p2 = b"second payload"
    buf = _sakar.encode_frame(_sakar.FRAME_RELAY, p1) + \
          _sakar.encode_frame(_sakar.FRAME_RELAY, p2)
    frames, rem = _sakar.decode_frames(buf)
    assert len(frames) == 2
    assert frames[0] == (_sakar.FRAME_RELAY, p1)
    assert frames[1] == (_sakar.FRAME_RELAY, p2)
    assert rem == b""


def test_frame_decode_partial():
    """Partial frame at end of buffer is kept as remainder."""
    full = _sakar.encode_frame(_sakar.FRAME_RELAY, b"hello")
    # Feed only first half.
    half = full[:len(full) // 2]
    frames, rem = _sakar.decode_frames(half)
    assert frames == []
    assert rem == half


def test_frame_decode_remainder():
    full = _sakar.encode_frame(_sakar.FRAME_RELAY, b"complete")
    extra = b"\x00\x00\x00"   # 3-byte partial prefix
    frames, rem = _sakar.decode_frames(full + extra)
    assert len(frames) == 1
    assert rem == extra


def test_frame_round_trip_large():
    payload = b"x" * 65000
    buf = _sakar.encode_frame(_sakar.FRAME_RELAY, payload)
    frames, rem = _sakar.decode_frames(buf)
    assert len(frames) == 1
    assert frames[0][1] == payload
    assert rem == b""


# ════════════════════════════════════════════════════════════════════════════
#  Process identity + relay meta (the connecting process shown in the prompt)
# ════════════════════════════════════════════════════════════════════════════

def test_read_peer_ident_self():
    info = _sakar.read_peer_ident(os.getpid())
    assert info["exe"] and "/" in info["exe"]          # readlink of /proc/self/exe
    assert isinstance(info["argv"], list) and info["argv"]

def test_read_peer_ident_dead_pid_is_empty():
    # A pid that doesn't exist degrades gracefully, never raises.
    info = _sakar.read_peer_ident(2 ** 31 - 1)
    assert info == {"exe": "", "argv": []}

def test_parse_relay_meta_proxy_default():
    kind, dest, proc = _sakar._parse_relay_meta(b"")
    assert kind == "proxy" and dest is None and proc == {}

def test_parse_relay_meta_carries_proc():
    payload = json.dumps({"kind": "proxy",
                          "proc": {"exe": "/usr/bin/curl",
                                   "argv": ["curl", "https://x"]}}).encode()
    kind, dest, proc = _sakar._parse_relay_meta(payload)
    assert kind == "proxy" and dest is None
    assert proc["exe"] == "/usr/bin/curl" and proc["argv"][0] == "curl"

def test_parse_relay_meta_direct_dest_and_proc():
    payload = json.dumps({"kind": "direct", "host": "example.com", "port": 443,
                          "scheme": "https",
                          "proc": {"exe": "/bin/wget", "argv": ["wget"]}}).encode()
    kind, dest, proc = _sakar._parse_relay_meta(payload)
    assert kind == "direct"
    assert dest == ("example.com", 443, "https")
    assert proc["exe"] == "/bin/wget"

def test_parse_relay_meta_malformed_fails_safe():
    kind, dest, proc = _sakar._parse_relay_meta(b"\xff\xff not json")
    assert kind == "proxy" and dest is None and proc == {}


# ════════════════════════════════════════════════════════════════════════════
#  CA bundle augmentation (OS roots + proxy CA, bound over the OS trust store)
# ════════════════════════════════════════════════════════════════════════════

def test_augment_bundle_concatenates_roots_then_ca():
    out = _sakar._augment_bundle(b"-----SYS ROOTS-----", b"-----PROXY CA-----")
    # OS roots first, our CA after, each on its own line, single trailing newline.
    assert out == b"-----SYS ROOTS-----\n-----PROXY CA-----\n"

def test_augment_bundle_does_not_fuse_pem_blocks():
    # Trailing/leading whitespace must not glue the END of one PEM to the BEGIN of
    # the next (which would make the combined block unparseable).
    out = _sakar._augment_bundle(b"AAA\n\n  ", b"\nBBB\n")
    assert out == b"AAA\nBBB\n"


# ════════════════════════════════════════════════════════════════════════════
#  Standalone runner
# ════════════════════════════════════════════════════════════════════════════

def check(cond, msg=""):
    if not cond:
        print(f"FAIL: {msg}", file=sys.stderr)
        sys.exit(1)
    print(f"ok: {msg}")

def _fails(fn, label=""):
    try:
        fn()
        return False
    except Exception:
        return True


if __name__ == "__main__":
    # DNS tests
    dns = _sakar.SyntheticDNS()
    pkt = _make_a_query(0xABCD, "example.com")
    resp = _sakar._dns_handle_packet(pkt, dns)
    check(resp is not None, "A query returns response")
    check(resp[-4] == 127, "A response is 127.x.x.x")

    check(_sakar._dns_handle_packet(b"", dns) is None, "empty pkt → None")
    check(_sakar._dns_handle_packet(b"\xff" * 20, dns) is None, "garbage → None")

    ip1 = dns.ip_for("stable.com")
    ip2 = dns.ip_for("stable.com")
    check(ip1 == ip2, "same domain → same IP")
    check(dns.domain_for(ip1) == "stable.com", "reverse lookup works")

    # Frame tests
    payload = b"test payload"
    enc = _sakar.encode_frame(_sakar.FRAME_RELAY, payload)
    frames, rem = _sakar.decode_frames(enc)
    check(len(frames) == 1 and frames[0][1] == payload, "frame round-trip")
    check(rem == b"", "no remainder")

    print("ALL STANDALONE CHECKS PASSED")
