#!/usr/bin/env -S uv run --with pytest --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["textual>=0.60","mitmproxy>=11","wcmatch>=8.4","pyfuse3>=3.2",
#                 "trio>=0.22","python-magic>=0.4"]
# ///
"""Unit tests for the three new features in sarun.

A. _GatingAddon.request sets X-Slopbox-Version on allowed flows and NOT on denied ones.
B. _decode_body_display: gzip, zlib-deflate, raw-deflate, multipart/form-data, brotli/unknown.
C. _hunks_display on a modified binary returns content + content_before; created has only content.

Run standalone:  ./test_flow_view.py
Run via pytest:  pytest test_flow_view.py -q
"""
import asyncio
import base64
import gzip
import os
import shutil
import stat as stat_mod
import sys
import tempfile
import types
import zlib
from pathlib import Path
from importlib.machinery import SourceFileLoader

SARUN = str(Path(__file__).resolve().parent / "sarun")
m = SourceFileLoader("slopbox", SARUN).load_module()

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


def _redirect_state(tmp):
    os.environ["XDG_STATE_HOME"] = str(tmp / "state")
    os.environ["XDG_RUNTIME_DIR"] = str(tmp / "run")
    os.environ["XDG_CONFIG_HOME"] = str(tmp / "config")
    os.environ["XDG_DATA_HOME"] = str(tmp / "data")


# ════════════════════════════════════════════════════════════════════════════
#  Feature A: _GatingAddon sets X-Slopbox-Version on allowed, not denied
# ════════════════════════════════════════════════════════════════════════════

def _fake_request(scheme="http", host="example.com", port=80, url="http://example.com/"):
    """Minimal fake mitmproxy flow.request with just enough attributes."""
    headers = {}
    req = types.SimpleNamespace(
        scheme=scheme, host=host, port=port,
        pretty_url=url, content=b"",
        method="GET",
        headers=headers,
    )
    flow = types.SimpleNamespace(
        request=req,
        metadata={},
        id="fake-flow-id-001",
        response=None,
    )
    return flow


def _fake_supervisor_allow():
    """Supervisor stub that always allows and has a no-op note_flow_open."""
    policy = types.SimpleNamespace(
        precheck=lambda *a, **kw: None,     # None → not denied
        approval_request=None,               # replaced with async allow below
    )
    sup = types.SimpleNamespace(
        policy=policy,
        note_flow_open=lambda *a, **kw: None,
    )
    async def _approval(*a, **kw):
        return {"action": "allow", "scope": "once"}
    policy.approval_request = _approval
    return sup


def _fake_supervisor_deny():
    """Supervisor stub that always denies."""
    policy = types.SimpleNamespace(
        precheck=lambda *a, **kw: "deny",
        approval_request=None,
    )
    sup = types.SimpleNamespace(
        policy=policy,
        note_flow_open=lambda *a, **kw: None,
    )
    from mitmproxy import http as _http
    async def _approval(*a, **kw):
        return {"action": "deny", "scope": "once"}
    policy.approval_request = _approval
    return sup


def test_addon_sets_version_on_allowed():
    """X-Slopbox-Version is set on an allowed request and equals VERSION."""
    sup = _fake_supervisor_allow()
    addon = m._GatingAddon(sup)
    flow = _fake_request(scheme="http")

    async def run():
        await addon.request(flow)

    asyncio.run(run())
    check(not flow.metadata.get("slopbox_denied"),
          "A: allowed flow is not denied")
    check(flow.request.headers.get("X-Slopbox-Version") == m.VERSION,
          f"A: X-Slopbox-Version == VERSION ({m.VERSION!r}) on allowed flow")


def test_addon_no_version_on_denied():
    """X-Slopbox-Version is NOT set on a denied request."""
    sup = _fake_supervisor_deny()
    addon = m._GatingAddon(sup)
    flow = _fake_request(scheme="http")

    async def run():
        await addon.request(flow)

    asyncio.run(run())
    check(flow.metadata.get("slopbox_denied") is True,
          "A: denied flow is marked slopbox_denied")
    check("X-Slopbox-Version" not in flow.request.headers,
          "A: X-Slopbox-Version absent on denied flow")


# ════════════════════════════════════════════════════════════════════════════
#  Feature B: _decode_body_display
# ════════════════════════════════════════════════════════════════════════════

def test_decode_gzip():
    """gzip-encoded body is decompressed and labelled 'gzip'."""
    original = b"hello world from gzip"
    compressed = gzip.compress(original)
    headers = "Content-Type: text/plain\nContent-Encoding: gzip"
    result = m._decode_body_display(compressed, headers)
    check(result is not None, "B: gzip returns non-None")
    if result:
        text, label = result
        check(text == original.decode("utf-8"), "B: gzip round-trips correctly")
        check(label == "gzip", "B: gzip label is 'gzip'")


def test_decode_zlib_deflate():
    """zlib-wrapped deflate body is decompressed and labelled 'deflate'."""
    original = b"hello world zlib deflate"
    compressed = zlib.compress(original)
    headers = "Content-Encoding: deflate\nContent-Type: text/plain"
    result = m._decode_body_display(compressed, headers)
    check(result is not None, "B: zlib-deflate returns non-None")
    if result:
        text, label = result
        check(text == original.decode("utf-8"), "B: zlib-deflate round-trips correctly")
        check(label == "deflate", "B: zlib-deflate label is 'deflate'")


def test_decode_raw_deflate():
    """Raw deflate (no zlib wrapper) body is decompressed and labelled 'deflate(raw)'."""
    original = b"hello world raw deflate"
    # raw deflate: compress then strip the 2-byte zlib header and 4-byte adler32 trailer
    raw_compressed = zlib.compress(original, level=6)
    raw_deflate = raw_compressed[2:-4]
    headers = "Content-Encoding: deflate\nContent-Type: text/plain"
    result = m._decode_body_display(raw_deflate, headers)
    check(result is not None, "B: raw-deflate returns non-None")
    if result:
        text, label = result
        check(text == original.decode("utf-8"), "B: raw-deflate round-trips correctly")
        check("deflate" in label, f"B: raw-deflate label contains 'deflate' (got {label!r})")


def test_decode_multipart():
    """multipart/form-data body is split into parts with per-part headers shown."""
    boundary = "----FormBoundaryXYZ"
    body = (
        f"------FormBoundaryXYZ\r\n"
        f"Content-Disposition: form-data; name=\"field1\"\r\n"
        f"\r\n"
        f"value1\r\n"
        f"------FormBoundaryXYZ\r\n"
        f"Content-Disposition: form-data; name=\"field2\"\r\n"
        f"\r\n"
        f"value2\r\n"
        f"------FormBoundaryXYZ--\r\n"
    ).encode("utf-8")
    headers = (f'Content-Type: multipart/form-data; boundary="----FormBoundaryXYZ"\r\n'
               f'Content-Length: {len(body)}')
    result = m._decode_body_display(body, headers)
    check(result is not None, "B: multipart returns non-None")
    if result:
        text, label = result
        check("part 1" in text, "B: multipart contains 'part 1'")
        check("part 2" in text, "B: multipart contains 'part 2'")
        check("value1" in text, "B: multipart part 1 body present")
        check("value2" in text, "B: multipart part 2 body present")
        check("multipart" in label, f"B: multipart label contains 'multipart' (got {label!r})")


def test_decode_multipart_base64_part():
    """A multipart part with Content-Transfer-Encoding: base64 (or quoted-printable) is
    transfer-decoded so its real content shows, not a wall of base64."""
    import base64 as _b64
    secret = "the real file contents — héllo"
    b64 = _b64.b64encode(secret.encode("utf-8")).decode("ascii")
    body = (
        "------B\r\n"
        'Content-Disposition: form-data; name="upload"; filename="a.txt"\r\n'
        "Content-Transfer-Encoding: base64\r\n"
        "\r\n"
        f"{b64}\r\n"
        "------B\r\n"
        'Content-Disposition: form-data; name="note"\r\n'
        "Content-Transfer-Encoding: quoted-printable\r\n"
        "\r\n"
        "caf=C3=A9 time\r\n"
        "------B--\r\n"
    ).encode("utf-8")
    headers = 'Content-Type: multipart/form-data; boundary="----B"'
    result = m._decode_body_display(body, headers)
    check(result is not None, "B: base64 multipart returns non-None")
    if result:
        text, _ = result
        check(secret in text, "B: base64 part is decoded to its real content")
        check(b64 not in text, "B: raw base64 string is NOT shown verbatim")
        check("café time" in text, "B: quoted-printable part decoded")
        check("base64" in text, "B: a per-part base64 tag is shown")


def test_decode_brotli_returns_none():
    """brotli Content-Encoding returns None (no dep — graceful skip)."""
    headers = "Content-Encoding: br\nContent-Type: text/plain"
    result = m._decode_body_display(b"\x1b\x00\x00", headers)
    check(result is None, "B: brotli returns None (no dep)")


def test_decode_unknown_encoding_returns_none():
    """Unknown Content-Encoding returns None."""
    headers = "Content-Encoding: zstd\nContent-Type: text/plain"
    result = m._decode_body_display(b"some bytes", headers)
    check(result is None, "B: unknown encoding returns None")


def test_decode_no_encoding_no_multipart_returns_none():
    """No Content-Encoding and no multipart body returns None (nothing to decode)."""
    headers = "Content-Type: text/plain\nContent-Length: 5"
    result = m._decode_body_display(b"hello", headers)
    check(result is None, "B: no encoding + no multipart returns None")


# ════════════════════════════════════════════════════════════════════════════
#  Feature C: _hunks_display binary before+after
# ════════════════════════════════════════════════════════════════════════════

def _make_cr_with_binary(tmp, rel, before_bytes, after_bytes):
    """Set up a finished box whose sqlar has `rel` as a binary file, and return
    (cr, sid) where cr is a ChangeReview that can call _hunks_display."""
    _redirect_state(tmp)
    sid = "7001"
    backing = m.live_dir(sid); (backing / "up").mkdir(parents=True, exist_ok=True)
    idx = m.Index(backing)
    wid = idx.writer_for(os.getpid())
    idx.set_entry(rel, "file", stat_mod.S_IFREG | 0o644, wid, "create")
    bp = m.blob_path(idx.box_id, idx.row_id(rel))
    bp.parent.mkdir(parents=True, exist_ok=True)
    bp.write_bytes(after_bytes)
    m.consolidate(str(backing), sid, index=idx)
    idx.close()

    sup = m.Supervisor(m.Rules(Path("/nonexistent")), mount=None)
    sup.sessions[sid] = m.Session(
        session_id=sid, box_id=int(sid), cmd=["sh"],
        live=False, shm_dir=str(backing))
    return sup.review, sid


def test_hunks_display_modified_binary_has_before_and_after():
    """A modified binary file returns both 'content' and 'content_before' in the diff dict."""
    tmp = Path(tempfile.mkdtemp(prefix="cr-bin-mod-"))
    # Use a real host file so host.exists() returns True (→ kind="modified").
    host_file = Path(f"/tmp/sarun_bintest_mod_{os.getpid()}.bin")
    before_bytes = bytes(range(16))     # binary (contains NUL)
    after_bytes  = bytes(range(16, 32)) # different binary bytes
    host_file.write_bytes(before_bytes)
    try:
        rel = host_file.relative_to("/").as_posix()
        cr, sid = _make_cr_with_binary(tmp, rel, before_bytes, after_bytes)
        src = cr._source(sid)
        result = cr._hunks_display(src, rel)
        d = result.get("diff") or {}
        check(d.get("kind") == "modified",
              "C: modified binary has kind='modified'")
        check("content" in d,
              "C: modified binary has 'content' key")
        check("content_before" in d,
              "C: modified binary has 'content_before' key")
        if "content" in d:
            got_after = base64.b64decode(d["content"])
            check(got_after == after_bytes,
                  "C: 'content' decodes to the after bytes")
        if "content_before" in d:
            got_before = base64.b64decode(d["content_before"])
            check(got_before == before_bytes,
                  "C: 'content_before' decodes to the before (host) bytes")
    finally:
        host_file.unlink(missing_ok=True)
        shutil.rmtree(tmp, ignore_errors=True)


def test_hunks_display_created_binary_no_content_before():
    """A created binary (no pre-existing host file) returns only 'content', no 'content_before'."""
    tmp = Path(tempfile.mkdtemp(prefix="cr-bin-cre-"))
    # Use a path that does NOT exist on the host.
    rel = f"tmp/sarun_bintest_created_{os.getpid()}.bin"
    host_file = Path("/") / rel
    after_bytes = bytes(range(32))
    assert not host_file.exists(), f"test setup: {host_file} must not exist"
    try:
        cr, sid = _make_cr_with_binary(tmp, rel, b"", after_bytes)
        src = cr._source(sid)
        result = cr._hunks_display(src, rel)
        d = result.get("diff") or {}
        check(d.get("kind") == "created",
              "C: created binary has kind='created'")
        check("content" in d,
              "C: created binary has 'content' key")
        check("content_before" not in d,
              "C: created binary does NOT have 'content_before' key")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


# ════════════════════════════════════════════════════════════════════════════
#  Main / pytest entry
# ════════════════════════════════════════════════════════════════════════════

def test_all():
    """Collect + run all check()-based tests; compatible with pytest."""
    test_addon_sets_version_on_allowed()
    test_addon_no_version_on_denied()
    test_decode_gzip()
    test_decode_zlib_deflate()
    test_decode_raw_deflate()
    test_decode_multipart()
    test_decode_brotli_returns_none()
    test_decode_unknown_encoding_returns_none()
    test_decode_no_encoding_no_multipart_returns_none()
    test_hunks_display_modified_binary_has_before_and_after()
    test_hunks_display_created_binary_no_content_before()
    if _fails:
        raise AssertionError(f"{len(_fails)} check(s) failed: {_fails}")


if __name__ == "__main__":
    test_all()
    if _fails:
        print(f"\n{len(_fails)} FAILED: {_fails}", file=sys.stderr)
        sys.exit(1)
    print(f"\nAll checks passed.")
