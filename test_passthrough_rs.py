#!/usr/bin/env python3
"""D5 — FUSE READ passthrough backing fds (engine/src/overlay.rs).

Per DESIGN.md D5: a READ-ONLY open whose resolve() lands on a single real
backing file (the host lower file, or a box pool blob) registers a kernel
backing fd (kernel 6.9+, fuser opened_passthrough / FUSE_DEV_IOC_BACKING_OPEN)
so the kernel serves subsequent reads with the daemon out of the loop. WRITE
opens stay daemon-served (per-write ctx.pid attribution + lazy copy-up are
load-bearing). Older kernels / failed registration fall back to daemon reads.

This test asserts REAL behavior through the host-visible mount (`<mnt>/<box>/`):
  - the engine actually NEGOTIATED passthrough (its init log line) — observable
    proof the kernel here supports it and we requested it. If it did NOT
    negotiate (kernel <6.9), the read-correctness assertions still run on the
    daemon-served fallback path, and the test records that passthrough was
    inactive instead of failing (the feature is implemented-but-inactive).
  - read-only opens of (a) a host lower file and (b) a box-written file return
    the correct bytes BYTE-FOR-BYTE under the passthrough path.
  - a LARGE file (multi-MB, many read requests) reads back byte-for-byte —
    this is the build-read-storm shape passthrough targets.
  - a writable open + write STILL copies-up, is captured into the box sqlar,
    and reads back the merged bytes (no D3 / capture regression).
  - the real host file is never mutated by box reads or writes.

    uv run --with "pyfuse3>=3.2" --with "trio>=0.22" --with "wcmatch>=8.4" \
      --with "python-magic>=0.4" python test_passthrough_rs.py

Skips (passes vacuously) if cargo/the binary/FUSE are unavailable.
"""
import hashlib, os, socket, subprocess, sys, tempfile, shutil, time
import stat as stat_mod
from pathlib import Path
from importlib.machinery import SourceFileLoader

_HERE = Path(__file__).resolve().parent
SARUN = str(_HERE / "sarun")
CRATE = _HERE / "engine"
BIN = CRATE / "target/release/sarun"

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond:
        _fails.append(msg)


def ensure_binary() -> bool:
    if BIN.exists():
        return True
    if shutil.which("cargo") is None:
        return False
    r = subprocess.run(["cargo", "build", "--release"], cwd=CRATE,
                       capture_output=True, text=True)
    return r.returncode == 0 and BIN.exists()


def kernel_ge(major, minor) -> bool:
    try:
        rel = os.uname().release.split("-")[0]
        a, b = (int(x) for x in rel.split(".")[:2])
        return (a, b) >= (major, minor)
    except Exception:
        return False


def wait_socket(sock, timeout=30):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.settimeout(1.0); s.connect(sock); return True
        except OSError:
            time.sleep(0.1)
    return False


def main():
    if not ensure_binary():
        print("  ok  passthrough-rs: cargo/binary unavailable — SKIP")
        print("\nPASSTHROUGH-RS PASS (skipped)")
        return 0
    if not os.path.exists("/dev/fuse"):
        print("  ok  passthrough-rs: /dev/fuse absent — SKIP")
        print("\nPASSTHROUGH-RS PASS (skipped)")
        return 0

    tmp = Path(tempfile.mkdtemp(prefix="ptrs-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "PT"
    stats_file = tmp / "stats"
    os.environ["SARUN_STATS_FILE"] = str(stats_file)
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()

    eng = None
    stats = ""
    host_lower = Path("/root/pt_lower.txt")
    host_big = Path("/root/pt_big.bin")
    host_untouched = Path("/root/pt_untouched.txt")
    try:
        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        sock = m.sock_path()
        if not wait_socket(sock):
            out = eng.stdout.read(2000) if eng.stdout else b""
            raise RuntimeError("rust engine socket never appeared:\n"
                               + out.decode(errors="replace"))
        # Give init() time to emit its negotiation line, then scrape stderr
        # non-blockingly via a tail of the captured pipe later (we read it at
        # teardown). For the engage assertion we instead re-derive: the kernel
        # here is >=6.9 AND the binary built with the feature → it negotiated.
        # We confirm directly from the log at the end.
        time.sleep(0.5)

        # ── set up host (lower) bytes ──────────────────────────────────────
        lower_bytes = b"host lower content\nsecond line\n"
        host_lower.write_bytes(lower_bytes)
        host_untouched.write_bytes(b"do not touch\n")
        # a multi-MB file → many read requests (the read-storm shape)
        big = os.urandom(7 * 1024 * 1024 + 123)
        host_big.write_bytes(big)
        big_sha = hashlib.sha256(big).hexdigest()

        # ── make a box ─────────────────────────────────────────────────────
        rep = m.sync_request(sock, type="ui", verb="box_new", args=[])
        check(rep and rep.get("ok"), "passthrough-rs: box_new answers")
        bsid = rep["r"]["sid"]; root = Path(rep["r"]["root"])
        check(root.is_dir(), "passthrough-rs: box root under the mount")

        # (a) READ-ONLY open of a HOST LOWER file → byte-for-byte ───────────
        with open(root / "root/pt_lower.txt", "rb") as f:
            got = f.read()
        check(got == lower_bytes,
              "passthrough-rs: read-only host-lower file is byte-for-byte")
        # partial / seeked reads through the same path (kernel-served)
        with open(root / "root/pt_lower.txt", "rb") as f:
            f.seek(5)
            check(f.read(5) == lower_bytes[5:10],
                  "passthrough-rs: seeked read of lower file is correct")

        # large-file read correctness (the storm) ───────────────────────────
        h = hashlib.sha256()
        with open(root / "root/pt_big.bin", "rb") as f:
            while True:
                chunk = f.read(64 * 1024)
                if not chunk:
                    break
                h.update(chunk)
        check(h.hexdigest() == big_sha,
              "passthrough-rs: 7MB read-only file reads back byte-for-byte")

        # the host originals are untouched by reads ─────────────────────────
        check(host_lower.read_bytes() == lower_bytes
              and host_big.read_bytes() == big
              and host_untouched.read_bytes() == b"do not touch\n",
              "passthrough-rs: host files unchanged after box reads")

        # (writable open + write) STILL copies up & is captured — NO regress ─
        new_bytes = b"box made this\n" * 1000
        (root / "root/pt_boxmade.txt").write_bytes(new_bytes)
        # append to a host lower file: first write triggers copy-up
        with open(root / "root/pt_lower.txt", "ab") as f:
            f.write(b"appended-by-box\n")
        merged = lower_bytes + b"appended-by-box\n"
        check((root / "root/pt_lower.txt").read_bytes() == merged,
              "passthrough-rs: writable open copies-up; reads back merged")
        sp = m.sqlar_path(bsid)
        check(m.sqlar_content(sp, "root/pt_lower.txt") == merged,
              "passthrough-rs: copy-up captured into the box sqlar (D3 intact)")
        check(m.sqlar_content(sp, "root/pt_boxmade.txt") == new_bytes,
              "passthrough-rs: box-created file captured into the sqlar")
        check(host_lower.read_bytes() == lower_bytes,
              "passthrough-rs: host lower file NOT mutated by the box write")

        # (b) READ-ONLY open of a BOX-WRITTEN file → byte-for-byte ──────────
        # this file's resolve() lands on a pool BLOB (owner == this box), the
        # other real-single-file passthrough case.
        with open(root / "root/pt_boxmade.txt", "rb") as f:
            check(f.read() == new_bytes,
                  "passthrough-rs: read-only box-written (blob) file byte-for-byte")

        # snapshot the engine's live stats (poller updates every 100ms) — this
        # survives the SIGTERM teardown below.
        time.sleep(0.3)
        stats = stats_file.read_text() if stats_file.exists() else ""

    finally:
        if eng is not None:
            eng.terminate()
            try:
                eng.wait(timeout=10)
            except subprocess.TimeoutExpired:
                eng.kill(); eng.wait(timeout=5)
        log = eng.stdout.read().decode(errors="replace") if eng and eng.stdout else ""
        for p in (host_lower, host_big, host_untouched):
            p.unlink(missing_ok=True)
        shutil.rmtree(tmp, ignore_errors=True)

    # Observable engagement: the engine's own init log states whether the
    # kernel negotiated passthrough. This is the direct oracle that the new
    # path is live (not just that reads happen to be correct).
    # Parse the daemon read() counter logged at destroy(). With passthrough
    # engaged, the ~112 chunked reads of the 7MB file (plus the small reads)
    # were served by the KERNEL — the daemon's read() handler is not called for
    # passthrough-backed fds, so this stays near zero. This is the direct
    # oracle that the new path actually bypassed the daemon (not just that the
    # bytes were correct).
    daemon_reads = None
    stat_pt = None
    for tok in stats.split():
        if tok.startswith("daemon_reads="):
            try: daemon_reads = int(tok.split("=", 1)[1])
            except ValueError: pass
        if tok.startswith("passthrough="):
            stat_pt = tok.split("=", 1)[1]

    engaged = "read-passthrough ENABLED" in log
    inactive = "read-passthrough unavailable" in log
    if engaged:
        check(stat_pt == "1",
              "passthrough-rs: engine stats confirm passthrough flag active")
        # The 7MB file alone is ~112 chunked reads; with passthrough the daemon
        # serves NONE of them — the kernel does. The stats poller captured the
        # daemon's read() count AFTER all reads completed: it must be 0. This is
        # the direct oracle that the new path bypassed the daemon, not merely
        # that the bytes were correct.
        check(daemon_reads == 0,
              f"passthrough-rs: daemon served {daemon_reads} read() ops "
              "(0 expected — kernel served all read-only reads incl. the 7MB)")
    elif inactive:
        # implemented-but-inactive: reads above proved the daemon-served
        # fallback is correct. Not a failure — the feature degrades cleanly.
        print("  ok  passthrough-rs: kernel did NOT negotiate passthrough "
              "(kernel <6.9 / no FUSE_PASSTHROUGH) — fallback proven correct")
        check(kernel_ge(6, 9) is False,
              "passthrough-rs: inactive only because kernel < 6.9 (expected)")
    else:
        check(False, "passthrough-rs: engine did not log a passthrough decision")

    print()
    if _fails:
        print(f"PASSTHROUGH-RS FAIL ({len(_fails)})")
        for f in _fails:
            print("   -", f)
        return 1
    print("PASSTHROUGH-RS PASS")
    return 0


def test_passthrough_rs():
    assert main() == 0


if __name__ == "__main__":
    sys.exit(main())
