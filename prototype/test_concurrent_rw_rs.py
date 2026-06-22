#!/usr/bin/env python3
"""Concurrent same-file read+write correctness (daemon-served overlay).

A captured file is held open read-only by one fd while another writer overwrites
it. This must WORK: the write succeeds, the box captures the new bytes, and the
HOST file is untouched (copy-up). It is the exact property that kernel FUSE
read-passthrough VIOLATED — an inode with a live passthrough fd rejects any new
write-open with EIO, with no daemon-side mitigation. That is why read-passthrough
was reverted (see DESIGN.md D5): the daemon serves reads so concurrent same-file
read+write stays correct. This test guards against reintroducing that footgun.

Run:
    uv run --with "pyfuse3>=3.2" --with "trio>=0.22" --with "wcmatch>=8.4" \
      --with "python-magic>=0.4" python test_concurrent_rw_rs.py
Skips (passes vacuously) if cargo/the binary/FUSE are unavailable.
"""
import os, shutil, socket, subprocess, sys, tempfile, time
from pathlib import Path
from importlib.machinery import SourceFileLoader

_HERE = Path(__file__).resolve().parent
SARUN = str(_HERE / "sarun")
CRATE = _HERE.parent / "engine"
BIN = CRATE / "target/x86_64-unknown-linux-musl/release/sarun"

ORIG = b"ORIGINAL-CONTENT-" + b"z" * 4000
NEW = b"WRITER-NEW"
CAP = Path("/root/crw_cap.bin")   # captured (copy-up on write)

# Hold a read fd open, then overwrite the SAME file while the read fd is live.
SH = r'''
exec 3</root/crw_cap.bin
if printf 'WRITER-NEW' > /root/crw_cap.bin 2>/root/r_err
then echo OK > /root/r_res; else echo FAIL > /root/r_res; fi
'''

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


def ensure_binary() -> bool:
    if BIN.exists():
        return True
    if shutil.which("cargo") is None:
        return False
    r = subprocess.run(["make", "engine"], cwd=CRATE.parent,
                       capture_output=True, text=True)
    return r.returncode == 0 and BIN.exists()


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
        raise SystemExit("test_concurrent_rw_rs: engine binary unavailable — run `make engine`")
    tmp = Path(tempfile.mkdtemp(prefix="crw-"))
    for k, sub in (("XDG_STATE_HOME", "st"), ("XDG_RUNTIME_DIR", "rn"),
                   ("XDG_CONFIG_HOME", "cf"), ("XDG_DATA_HOME", "d")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "CRW"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    CAP.write_bytes(ORIG)
    eng = None
    try:
        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if not wait_socket(m.sock_path()):
            raise RuntimeError("engine socket never appeared")
        subprocess.run([str(BIN), "run", "--", "sh", "-c", SH],
                       capture_output=True, text=True, timeout=120)
        sps = list(Path(os.environ["XDG_STATE_HOME"]).joinpath(
            "slopbox.CRW").glob("*.sqlar"))
        sp = max(sps, key=lambda p: int(p.stem)) if sps else None
        g = (lambda n: m.sqlar_content(sp, n)) if sp else (lambda n: None)

        # The write to a file with a live read fd SUCCEEDS — no EIO. (Kernel
        # read-passthrough would EIO here; that is why it was reverted.)
        check((g("root/r_res") or b"").strip() == b"OK",
              "crw-rs: write to a file with a live read fd SUCCEEDS (no EIO) "
              f"err={g('root/r_err')!r}")
        check(g("root/crw_cap.bin") == NEW,
              "crw-rs: box captured the writer's new bytes")
        check(CAP.read_bytes() == ORIG,
              "crw-rs: HOST untouched (copy-up, not in-place)")
    finally:
        CAP.unlink(missing_ok=True)
        if eng is not None and eng.poll() is None:
            eng.terminate()
            try: eng.wait(timeout=10)
            except Exception: eng.kill()
        os.environ.pop("SLOPBOX_NS", None)
        shutil.rmtree(tmp, ignore_errors=True)
    print("\n" + ("CRW-RS PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_concurrent_rw_rs():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
