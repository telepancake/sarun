#!/usr/bin/env python3
"""Backend tests for stdout/stderr capture.

Three layers:
  1. muxed-channel frame codec — encode→decode round-trip (ECHO/ECHO_DONE/MUTE/
     UNMUTE types) + partial-frame reassembly (pure, no socket).
  2. outputs_* sqlar readers — add/list/get/has against a throwaway db.
  3. REAL FUSE integration — mount the multiplexed overlay, register a capture
     session so the two sink files exist, open a sink and write to it FROM THIS
     PROCESS, and assert an `outputs` row was recorded whose process_id resolves
     to this process and whose stream/content match. This proves per-write
     attribution end-to-end through the patched pyfuse3 (the write handler's
     ctx.pid is the real writer's pid). A second FUSE case adds THIS process's pid
     to the global muted set and asserts a muted write is NOT recorded.

The full bwrap run_inner↔channel path (child stdout/stderr → sinks, ECHO frames
replayed, MUTE/UNMUTE bracketing) is exercised only by the excluded e2e suite — it
needs a real box.

    uv run --with pyfuse3 --with trio pytest test_outputs_capture.py
"""
import os
import shutil
import sys
import tempfile
import time
from importlib.machinery import SourceFileLoader
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent / "bench"))
import extsuite

SARUN = str(Path(__file__).resolve().parent / "sarun")
m = SourceFileLoader("slopbox", SARUN).load_module()

_fails = []


def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond:
        _fails.append(msg)


# ── 1. muxed-channel frame codec ─────────────────────────────────────────────
def test_frame_codec_roundtrip():
    # All frame types, including empty payloads and binary, round-trip.
    frames = [
        (m.FRAME_ECHO, m.echo_payload(0, b"hello")),
        (m.FRAME_ECHO, m.echo_payload(1, b"\x00\x01\x02\xff")),
        (m.FRAME_ECHO_DONE, b""),
        (m.FRAME_MUTE, b""),
        (m.FRAME_UNMUTE, b""),
    ]
    buf = b"".join(m.encode_frame(t, p) for t, p in frames)
    got, rem = m.decode_frames(buf)
    check(got == frames, "encode→decode round-trips every (type, payload)")
    check(rem == b"", "no remainder when buffer ends on a frame boundary")
    p = m.echo_payload(1, b"abc")
    check(p[0] == 1 and p[1:] == b"abc", "echo_payload encodes [stream][bytes]")


def test_frame_codec_partial_reassembly():
    frames = [(m.FRAME_ECHO, m.echo_payload(0, b"chunk-one")),
              (m.FRAME_MUTE, b"")]
    full = b"".join(m.encode_frame(t, p) for t, p in frames)
    # Feed the stream one byte at a time; a frame split across reads must reassemble.
    out = []
    carry = b""
    for i in range(len(full)):
        carry += full[i:i + 1]
        got, carry = m.decode_frames(carry)
        out.extend(got)
    check(out == frames, "byte-at-a-time feed reassembles split frames")
    check(carry == b"", "no leftover after the last full frame")

    # A truncated trailing frame is held as remainder, not mis-decoded.
    f0 = m.encode_frame(*frames[0])
    got, rem = m.decode_frames(full[:-3])
    check(got == frames[:1], "first whole frame decoded; partial second withheld")
    check(rem == full[len(f0):-3],
          "the partial second frame is returned verbatim as remainder")


# ── 2. outputs_* readers ─────────────────────────────────────────────────────
def test_outputs_readers():
    tmp = Path(tempfile.mkdtemp(prefix="outputs-"))
    try:
        db = tmp / "1.sqlar"
        check(m.has_outputs(db) is False, "has_outputs false before any db exists")
        m.outputs_add(db, dict(ts=100.0, process_id=7, stream=0, content=b"out-a"))
        m.outputs_add(db, dict(ts=101.0, process_id=7, stream=1, content=b"err-bb"))
        check(m.has_outputs(db) is True, "has_outputs true after a row is added")
        rows = m.outputs_list(db)
        check(len(rows) == 2, "outputs_list returns every row")
        check(rows[0]["stream"] == 0 and rows[1]["stream"] == 1,
              "outputs_list preserves stream per row")
        check(rows[0]["len"] == 5 and rows[1]["len"] == 6,
              "outputs_list reports length(content), not content")
        check("content" not in rows[0], "outputs_list omits the content blob")
        det = m.outputs_get(db, rows[1]["id"])
        check(det is not None and det["content"] == b"err-bb",
              "outputs_get returns the full content blob")
        check(det["process_id"] == 7, "outputs_get carries process_id")
        check(m.outputs_get(db, 9999) is None, "outputs_get returns None for a missing id")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


# ── 3. real FUSE integration: per-write attribution through patched pyfuse3 ───
def _resolve_tgid(index, process_id):
    """tgid recorded for a process-table row id (RAM mirror, then db fallback)."""
    info = index.proc_info(process_id)
    if info is not None:
        return info[0]
    row = index._db.execute("SELECT tgid FROM process WHERE id=?",
                            (process_id,)).fetchone()
    return row[0] if row else None


def test_sink_write_records_attributed_output():
    extsuite.require_fuse()
    tmproot = tempfile.mkdtemp(prefix="outputs-fuse-")
    os.environ["XDG_STATE_HOME"] = os.path.join(tmproot, "state")
    os.environ["XDG_RUNTIME_DIR"] = os.path.join(tmproot, "run")
    os.makedirs(os.environ["XDG_STATE_HOME"], exist_ok=True)
    os.makedirs(os.environ["XDG_RUNTIME_DIR"], exist_ok=True)
    mount = m.OverlayMount(m.mnt_point(), lower="/")
    if not mount.start():
        shutil.rmtree(tmproot, ignore_errors=True)
        raise RuntimeError(f"overlay mount failed: {mount._start_error}")
    try:
        sid = str(m.mint_box_id())
        backing = m.live_dir(sid)
        (backing / "up").mkdir(parents=True, exist_ok=True)
        index = m.Index(backing)
        mount.add_session(sid, backing / "up", index)
        # Register the two capture sinks exactly as the register handler does.
        mount.ops.add_sink(sid, m.SINK_STDOUT_REL, 0)
        mount.ops.add_sink(sid, m.SINK_STDERR_REL, 1)
        box_root = m.mnt_point() / sid

        # Write to BOTH sinks from THIS process, through the real mount.
        out_payload = b"captured stdout line\n"
        err_payload = b"captured stderr line\n"
        with open(box_root / m.SINK_STDOUT_REL, "wb", buffering=0) as f:
            f.write(out_payload)
        with open(box_root / m.SINK_STDERR_REL, "wb", buffering=0) as f:
            f.write(err_payload)

        # The sink files must NOT appear in a directory listing (lookup-only).
        listed = set(os.listdir(box_root))
        check(m.SINK_STDOUT_REL not in listed and m.SINK_STDERR_REL not in listed,
              "sink files are hidden from readdir (resolvable by exact name only)")

        rows = m.outputs_list(m.sqlar_path(sid))
        check(len(rows) == 2, "two outputs rows recorded (one stdout, one stderr)")
        by_stream = {r["stream"]: r for r in rows}
        check(set(by_stream) == {0, 1}, "both streams (0=stdout, 1=stderr) recorded")

        out_row = m.outputs_get(m.sqlar_path(sid), by_stream[0]["id"])
        err_row = m.outputs_get(m.sqlar_path(sid), by_stream[1]["id"])
        check(out_row["content"] == out_payload, "stdout content recorded verbatim")
        check(err_row["content"] == err_payload, "stderr content recorded verbatim")

        # PER-WRITE ATTRIBUTION: process_id must resolve to THIS process's tgid —
        # proving the write handler saw the real writer's ctx.pid (patched pyfuse3).
        my_tgid = os.getpid()
        check(out_row["process_id"] is not None,
              "stdout row is attributed to a process row")
        check(_resolve_tgid(index, out_row["process_id"]) == my_tgid,
              "stdout writer resolves to THIS process (per-write ctx.pid attribution)")
        check(_resolve_tgid(index, err_row["process_id"]) == my_tgid,
              "stderr writer resolves to THIS process (per-write ctx.pid attribution)")
    finally:
        try: mount.stop()
        except Exception: pass
        shutil.rmtree(tmproot, ignore_errors=True)


def test_muted_sink_write_not_recorded():
    """A write whose host pid is in the global muted set is NOT recorded (no outputs
    row) even though it still flows through the sink — this is the nested-echo mute:
    readback travelling up an ancestor sink is echoed onward but not re-recorded."""
    extsuite.require_fuse()
    tmproot = tempfile.mkdtemp(prefix="outputs-mute-")
    os.environ["XDG_STATE_HOME"] = os.path.join(tmproot, "state")
    os.environ["XDG_RUNTIME_DIR"] = os.path.join(tmproot, "run")
    os.makedirs(os.environ["XDG_STATE_HOME"], exist_ok=True)
    os.makedirs(os.environ["XDG_RUNTIME_DIR"], exist_ok=True)
    mount = m.OverlayMount(m.mnt_point(), lower="/")
    if not mount.start():
        shutil.rmtree(tmproot, ignore_errors=True)
        raise RuntimeError(f"overlay mount failed: {mount._start_error}")
    m._MUTED_HOST_PIDS.add(os.getpid())
    try:
        sid = str(m.mint_box_id())
        backing = m.live_dir(sid)
        (backing / "up").mkdir(parents=True, exist_ok=True)
        index = m.Index(backing)
        mount.add_session(sid, backing / "up", index)
        mount.ops.add_sink(sid, m.SINK_STDOUT_REL, 0)
        box_root = m.mnt_point() / sid
        with open(box_root / m.SINK_STDOUT_REL, "wb", buffering=0) as f:
            f.write(b"muted output should NOT be recorded\n")
        rows = m.outputs_list(m.sqlar_path(sid))
        check(rows == [] or len(rows) == 0,
              "a muted writer's sink write is NOT recorded in outputs")
    finally:
        m._MUTED_HOST_PIDS.discard(os.getpid())
        try: mount.stop()
        except Exception: pass
        shutil.rmtree(tmproot, ignore_errors=True)


if __name__ == "__main__":
    tests = [test_frame_codec_roundtrip, test_frame_codec_partial_reassembly,
             test_outputs_readers]
    try:
        extsuite.require_fuse()
        tests.append(test_sink_write_records_attributed_output)
        tests.append(test_muted_sink_write_not_recorded)
    except extsuite._Skip as e:
        print(f"  skip  FUSE integration ({e})")
    for t in tests:
        try:
            t()
        except extsuite._Skip as e:
            print(f"  skip  {t.__name__} ({e})")
        except Exception:
            import traceback
            traceback.print_exc()
            _fails.append(t.__name__)
    print("\n" + ("ALL PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    sys.exit(1 if _fails else 0)
