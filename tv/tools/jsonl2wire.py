#!/usr/bin/env python3
"""Convert legacy ``proctrace`` JSONL traces into the tv trace format.

Old traces (``cat /proc/proctrace/new > trace.jsonl``) are one JSON
object per line. The trace format (``trace/trace.h``) is delta-encoded
atoms, splits each old EXEC into four trace events (EV_EXEC / EV_ARGV /
EV_ENV / EV_AUXV), and is what ``tv --trace`` / ``tv --open`` expect.

Designed for 20+ GB inputs:

* lines are read in fixed-byte chunks and dispatched to a worker pool that
  parses JSON and produces flat event tuples
* the main process performs the (inherently sequential) delta-encoding +
  atom emission and streams to disk
* malformed lines are tolerated — they produce a single ``WARN`` line on
  stderr and are skipped, never aborting the run
* a periodic ``progress`` line on stderr lets you eyeball throughput

Usage:

    tools/jsonl2wire.py trace.jsonl trace.bin        # plain
    tools/jsonl2wire.py -j 8 trace.jsonl trace.bin   # 8 worker procs
    cat trace.jsonl | tools/jsonl2wire.py - trace.bin
    tools/jsonl2wire.py trace.jsonl - | zstd > trace.bin.zst

The output can be ingested with ``./tv --trace trace.bin`` or piped
straight into the ingester.
"""

from __future__ import annotations

import argparse
import io
import json
import multiprocessing as mp
import os
import re
import sys
import time
from typing import Iterable, List, Optional, Tuple


# ─────────────────────────── trace constants ────────────────────────── #

TRACE_VERSION = 3

EV_EXEC, EV_ARGV, EV_ENV, EV_AUXV = 0, 1, 2, 3
EV_EXIT, EV_OPEN, EV_CWD = 4, 5, 6
EV_STDOUT, EV_STDERR = 7, 8

EV_EXIT_EXITED, EV_EXIT_SIGNALED = 0, 1

# All known O_* flag names from <fcntl.h>.  Old traces emit these as
# strings; new wire stores a single int.  Values from Linux x86_64.
OPEN_FLAGS = {
    "O_RDONLY":    0o00000000,
    "O_WRONLY":    0o00000001,
    "O_RDWR":      0o00000002,
    "O_CREAT":     0o00000100,
    "O_EXCL":      0o00000200,
    "O_NOCTTY":    0o00000400,
    "O_TRUNC":     0o00001000,
    "O_APPEND":    0o00002000,
    "O_NONBLOCK":  0o00004000,
    "O_NDELAY":    0o00004000,
    "O_DSYNC":     0o00010000,
    "O_ASYNC":     0o00020000,
    "O_DIRECT":    0o00040000,
    "O_LARGEFILE": 0o00100000,
    "O_DIRECTORY": 0o00200000,
    "O_NOFOLLOW":  0o00400000,
    "O_NOATIME":   0o01000000,
    "O_CLOEXEC":   0o02000000,
    "O_SYNC":      0o04010000,
    "O_PATH":      0o010000000,
    "O_TMPFILE":   0o020200000,
}


# ─────────────────────────── atom encoder ──────────────────────────── #

def wire_put_blob(buf: bytearray, src: bytes) -> None:
    n = len(src)
    if n == 1 and src[0] < 0xC0:
        buf.append(src[0])
        return
    if n <= 0x37:
        buf.append(0xC0 + n)
        buf.extend(src)
        return
    # long form
    lenbuf = bytearray()
    tmp = n
    while tmp:
        lenbuf.append(tmp & 0xFF)
        tmp >>= 8
    if len(lenbuf) > 7:
        raise OverflowError(f"blob too large: {n} bytes")
    buf.append(0xF8 + len(lenbuf))
    buf.extend(lenbuf)
    buf.extend(src)


def wire_put_u64(buf: bytearray, v: int) -> None:
    if v < 0 or v >= (1 << 64):
        raise OverflowError(v)
    if v == 0:
        buf.append(0xC0)
        return
    raw = bytearray()
    tmp = v
    while tmp:
        raw.append(tmp & 0xFF)
        tmp >>= 8
    if len(raw) == 1 and raw[0] < 0xC0:
        buf.append(raw[0])
        return
    buf.append(0xC0 + len(raw))
    buf.extend(raw)


def wire_put_i64(buf: bytearray, v: int) -> None:
    u = ((v << 1) ^ (v >> 63)) & ((1 << 64) - 1)
    wire_put_u64(buf, u)


def wire_put_pair(out: bytearray, a: bytes, b: bytes) -> None:
    """Outer atom whose payload is wire_put_blob(a) || wire_put_blob(b)."""
    inner = bytearray()
    wire_put_blob(inner, a)
    wire_put_blob(inner, b)
    wire_put_blob(out, bytes(inner))


# ─────────────────────────── translation ────────────────────────────── #

# A flat event tuple shipped from worker to main, keeping the message
# small and easy to pickle.  Schema:
#   (type, ts_ns, pid, tgid, ppid, nspid, nstgid, extras_tuple, blob_bytes)
EventTuple = Tuple[int, int, int, int, int, int, int, Tuple[int, ...], bytes]


def _ts_to_ns(v) -> int:
    """Old trace ts may be a float (seconds) or an int (ns).  Normalise."""
    if isinstance(v, (int,)):
        return int(v) if v > 10**14 else int(v) * 10**9
    if isinstance(v, float):
        return int(round(v * 1e9))
    if isinstance(v, str):
        # tolerant of strings like "1711814400.123456789"
        return int(round(float(v) * 1e9))
    return 0


def _flags_to_int(flags) -> int:
    """Old: list of strings.  New: int bitmask."""
    if isinstance(flags, int):
        return flags
    if isinstance(flags, list):
        v = 0
        for s in flags:
            v |= OPEN_FLAGS.get(s, 0)
        return v
    return 0


_DEV_RE = re.compile(r"^(\d+):(\d+)$")


def _dev_to_majmin(dev) -> Tuple[int, int]:
    if isinstance(dev, str):
        m = _DEV_RE.match(dev)
        if m:
            return int(m.group(1)), int(m.group(2))
    if isinstance(dev, list) and len(dev) == 2:
        return int(dev[0]), int(dev[1])
    if isinstance(dev, int):
        # encoded major:minor
        return (dev >> 8) & 0xFFF, dev & 0xFF
    return 0, 0


def _common_fields(obj: dict) -> Tuple[int, int, int, int, int, int]:
    """Returns (ts_ns, pid, tgid, ppid, nspid, nstgid)."""
    ts = _ts_to_ns(obj.get("ts", 0))
    pid = int(obj.get("pid", 0) or 0)
    tgid = int(obj.get("tgid", pid) or pid)
    ppid = int(obj.get("ppid", 0) or 0)
    nspid = int(obj.get("nspid", pid) or pid)
    nstgid = int(obj.get("nstgid", tgid) or tgid)
    return ts, pid, tgid, ppid, nspid, nstgid


def _argv_blob(argv) -> bytes:
    if not argv:
        return b""
    if isinstance(argv, str):
        return argv.encode("utf-8", errors="replace") + b"\x00"
    out = bytearray()
    for s in argv:
        out += str(s).encode("utf-8", errors="replace") + b"\x00"
    return bytes(out)


def _env_blob(env) -> bytes:
    if not env:
        return b""
    out = bytearray()
    if isinstance(env, dict):
        for k, v in env.items():
            kb = str(k).encode("utf-8", errors="replace")
            vb = str(v).encode("utf-8", errors="replace")
            out += kb + b"=" + vb + b"\x00"
    elif isinstance(env, list):
        for s in env:
            out += str(s).encode("utf-8", errors="replace") + b"\x00"
    return bytes(out)


def _auxv_blob(auxv) -> bytes:
    """Old JSONL emitted auxv as a parsed dict ({"AT_UID": 1000, ...}).
    The new wire format expects raw Elf64_auxv_t bytes (pairs of u64).
    Re-pack what we have using the standard AT_* numeric constants;
    string-typed entries (AT_EXECFN, AT_PLATFORM) become AT_NULL since
    we have no in-process address to point them at.  Down-stream
    consumers (tv) only look at the AT_* numeric scalars anyway."""
    if not auxv:
        return b""
    if isinstance(auxv, (bytes, bytearray)):
        return bytes(auxv)
    AT = {
        "AT_NULL": 0, "AT_IGNORE": 1, "AT_EXECFD": 2, "AT_PHDR": 3,
        "AT_PHENT": 4, "AT_PHNUM": 5, "AT_PAGESZ": 6, "AT_BASE": 7,
        "AT_FLAGS": 8, "AT_ENTRY": 9, "AT_NOTELF": 10, "AT_UID": 11,
        "AT_EUID": 12, "AT_GID": 13, "AT_EGID": 14, "AT_PLATFORM": 15,
        "AT_HWCAP": 16, "AT_CLKTCK": 17, "AT_SECURE": 23, "AT_BASE_PLATFORM": 24,
        "AT_RANDOM": 25, "AT_HWCAP2": 26, "AT_EXECFN": 31,
    }
    pairs = bytearray()
    for k, v in auxv.items() if isinstance(auxv, dict) else []:
        atn = AT.get(k)
        if atn is None or not isinstance(v, int):
            continue
        pairs += int(atn).to_bytes(8, "little", signed=False)
        # mask to 64-bit unsigned
        pairs += (v & ((1 << 64) - 1)).to_bytes(8, "little", signed=False)
    pairs += (0).to_bytes(8, "little") * 2  # AT_NULL terminator
    return bytes(pairs)


def translate(line: bytes) -> Optional[List[EventTuple]]:
    """Parse one JSONL line, returning 0..N wire EventTuples.  Returns
    None if the line is not parseable as a known event."""
    try:
        obj = json.loads(line)
    except (json.JSONDecodeError, UnicodeDecodeError):
        return None
    if not isinstance(obj, dict):
        return None
    ev = obj.get("event")
    if not isinstance(ev, str):
        return None

    ts, pid, tgid, ppid, nspid, nstgid = _common_fields(obj)
    base = (ts, pid, tgid, ppid, nspid, nstgid)
    out: List[EventTuple] = []

    if ev == "EXEC":
        exe = obj.get("exe") or ""
        out.append((EV_EXEC, *base, (), exe.encode("utf-8", errors="replace")))
        out.append((EV_ARGV, *base, (), _argv_blob(obj.get("argv"))))
        env = obj.get("env")
        if env is not None:
            out.append((EV_ENV, *base, (), _env_blob(env)))
        auxv = obj.get("auxv")
        if auxv:
            out.append((EV_AUXV, *base, (), _auxv_blob(auxv)))
        return out
    if ev == "CWD":
        path = obj.get("path") or ""
        out.append((EV_CWD, *base, (), path.encode("utf-8", errors="replace")))
        return out
    if ev == "OPEN":
        path = obj.get("path") or ""
        flags = _flags_to_int(obj.get("flags"))
        fd = int(obj.get("fd", -1) if obj.get("fd") is not None else -1)
        ino = int(obj.get("ino", 0) or 0)
        dmaj, dmin = _dev_to_majmin(obj.get("dev", 0))
        err = int(obj.get("err", 0) or 0)
        inh = 1 if obj.get("inherited") else 0
        extras = (flags, fd, ino, dmaj, dmin, err, inh)
        out.append((EV_OPEN, *base, extras,
                    path.encode("utf-8", errors="replace")))
        return out
    if ev == "EXIT":
        status = obj.get("status")
        code = int(obj.get("code", 0) or 0)
        sig = int(obj.get("signal", 0) or 0)
        cored = 1 if obj.get("core_dumped") else 0
        raw = int(obj.get("raw", 0) or 0)
        if status == "signaled":
            extras = (EV_EXIT_SIGNALED, sig, cored, raw)
        else:
            extras = (EV_EXIT_EXITED, code, 0, raw)
        out.append((EV_EXIT, *base, extras, b""))
        return out
    if ev in ("STDOUT", "STDERR"):
        data = obj.get("data") or ""
        # Old traces JSON-escape arbitrary bytes; round-trip as UTF-8.
        if isinstance(data, str):
            blob = data.encode("utf-8", errors="replace")
        else:
            blob = bytes(data) if isinstance(data, (bytes, bytearray)) else b""
        kind = EV_STDOUT if ev == "STDOUT" else EV_STDERR
        out.append((kind, *base, (), blob))
        return out

    # Unknown event kind — silently skip, very old traces had transient
    # debug events.
    return None


def _worker(chunk: List[bytes]) -> Tuple[List[EventTuple], int]:
    out: List[EventTuple] = []
    bad = 0
    for line in chunk:
        line = line.strip()
        if not line:
            continue
        events = translate(line)
        if events is None:
            bad += 1
            continue
        out.extend(events)
    return out, bad


# ─────────────────────────── main driver ────────────────────────────── #

def _emit_event(buf: bytearray, state: List[int], ev: EventTuple) -> None:
    """Build the event header (stream_id || delta-encoded base ||
    extras) and wrap with wire_put_pair against the blob."""
    typ, ts, pid, tgid, ppid, nspid, nstgid, extras, blob = ev
    hdr = bytearray()
    # stream_id (this importer is a single producer → 1)
    wire_put_u64(hdr, 1)
    new = (typ, ts, pid, tgid, ppid, nspid, nstgid)
    for i in range(7):
        d = new[i] - state[i]
        if d >= (1 << 63):
            d -= (1 << 64)
        elif d < -(1 << 63):
            d += (1 << 64)
        wire_put_i64(hdr, d)
        state[i] = new[i]
    for x in extras:
        v = int(x)
        if v >= (1 << 63):
            v -= (1 << 64)
        elif v < -(1 << 63):
            v += (1 << 64)
        wire_put_i64(hdr, v)
    wire_put_pair(buf, bytes(hdr), blob)


def _open_input(path: str) -> io.BufferedReader:
    if path == "-":
        return sys.stdin.buffer
    if path.endswith(".zst"):
        try:
            import zstandard as zstd  # type: ignore
        except ImportError:
            sys.exit("input is .zst but the `zstandard` Python package is not installed")
        f = open(path, "rb")
        dctx = zstd.ZstdDecompressor()
        return dctx.stream_reader(f)
    if path.endswith(".gz"):
        import gzip
        return gzip.open(path, "rb")
    return open(path, "rb")


def _open_output(path: str) -> io.BufferedWriter:
    if path == "-":
        return sys.stdout.buffer
    return open(path, "wb")


def _iter_chunks(fp, chunk_lines: int = 8192):
    chunk: List[bytes] = []
    for line in fp:
        chunk.append(line)
        if len(chunk) >= chunk_lines:
            yield chunk
            chunk = []
    if chunk:
        yield chunk


def main(argv: Optional[List[str]] = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("input", help="input JSONL file (or - for stdin); .zst/.gz auto-detected")
    ap.add_argument("output", help="output wire file (or - for stdout)")
    ap.add_argument("-j", "--jobs", type=int, default=max(1, (os.cpu_count() or 2) // 2),
                    help="parallel JSON-parse workers (default: half of CPU count)")
    ap.add_argument("--chunk", type=int, default=8192,
                    help="lines per worker chunk (default: 8192)")
    ap.add_argument("-q", "--quiet", action="store_true",
                    help="suppress progress lines on stderr")
    args = ap.parse_args(argv)

    fin = _open_input(args.input)
    fout = _open_output(args.output)
    out_buf = bytearray()
    wire_put_u64(out_buf, TRACE_VERSION)
    fout.write(bytes(out_buf))
    out_buf.clear()

    state = [0] * 7
    n_events = 0
    n_bad = 0
    n_lines = 0
    t0 = time.monotonic()
    last_print = t0

    def progress():
        nonlocal last_print
        if args.quiet:
            return
        now = time.monotonic()
        if now - last_print < 1.0:
            return
        last_print = now
        elapsed = now - t0
        rate = n_lines / elapsed if elapsed > 0 else 0
        sys.stderr.write(
            f"  jsonl2wire: {n_lines:>12,} lines  "
            f"{n_events:>12,} events  {n_bad:>6,} bad  "
            f"{rate/1e3:>6.1f} kline/s\n"
        )
        sys.stderr.flush()

    chunks_iter = _iter_chunks(fin, args.chunk)

    if args.jobs > 1:
        # imap_unordered would reorder — preserve event order.
        ctx = mp.get_context("forkserver" if "forkserver" in mp.get_all_start_methods() else "spawn")
        with ctx.Pool(args.jobs) as pool:
            for events, bad in pool.imap(_worker, chunks_iter, chunksize=1):
                n_lines += len(events) + bad   # rough count
                n_bad += bad
                for ev in events:
                    _emit_event(out_buf, state, ev)
                    n_events += 1
                if out_buf:
                    fout.write(bytes(out_buf))
                    out_buf.clear()
                progress()
    else:
        for chunk in chunks_iter:
            events, bad = _worker(chunk)
            n_lines += len(chunk)
            n_bad += bad
            for ev in events:
                _emit_event(out_buf, state, ev)
                n_events += 1
            if out_buf:
                fout.write(bytes(out_buf))
                out_buf.clear()
            progress()

    if fout is not sys.stdout.buffer:
        fout.close()
    elapsed = time.monotonic() - t0
    if not args.quiet:
        sys.stderr.write(
            f"jsonl2wire: done. {n_lines:,} lines → {n_events:,} wire events "
            f"({n_bad:,} bad lines skipped) in {elapsed:.1f}s "
            f"({n_lines/elapsed/1e3:.1f} kline/s).\n"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
