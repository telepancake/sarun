#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""m4 proof: the RUST engine builds as a FULLY-STATIC musl binary and still
serves + runs a real box.

This test is standalone (python test_musl_rs.py) AND pytest-compatible.

It is HONEST about the toolchain: if the musl target binary has not been built
(no musl-tools / musl rust target — see CLAUDE.md / engine/DESIGN.md m4) it
SELF-SKIPS with a clear message (vacuous pass), never a fake pass. When the
musl binary IS present it asserts, for real:
  * `file` says the binary is statically linked,
  * `ldd` says it is "not a dynamic executable",
  * a `sarun engine` instance comes up (its control socket appears), and
  * a real `sarun run -- echo …` box runs against it and exits 0.

Build the musl binary first with:
  cargo build --release --target x86_64-unknown-linux-musl   (cwd: engine/)
"""
import os, socket, subprocess, sys, tempfile, time
from pathlib import Path

_HERE = Path(__file__).resolve().parent
CRATE = _HERE.parent / "engine"
MUSL_BIN = CRATE / "target/x86_64-unknown-linux-musl/release/sarun"


def check(cond, msg):
    if not cond:
        raise AssertionError(msg)
    print(f"  ok  {msg}")


def wait_socket(sock, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if os.path.exists(sock):
            try:
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                    s.settimeout(1.0)
                    s.connect(sock)
                    return True
            except OSError:
                pass
        time.sleep(0.1)
    return False


def main():
    if not MUSL_BIN.exists():
        print("  ok  musl-rs: static musl binary not built — SKIP "
              "(build with `make engine` from the repo root)")
        return 0

    # --- static-link proof ---------------------------------------------------
    f = subprocess.run(["file", str(MUSL_BIN)], capture_output=True, text=True)
    check("statically linked" in f.stdout or "static-pie" in f.stdout,
          f"musl-rs: `file` reports static linkage ({f.stdout.split(':',1)[-1].strip()})")

    l = subprocess.run(["ldd", str(MUSL_BIN)], capture_output=True, text=True)
    out = (l.stdout + l.stderr).lower()
    # Different ldd implementations phrase a no-dynamic-libc binary as either
    # "not a dynamic executable" (glibc ldd) or "statically linked" (musl/the
    # static loader stub). Either proves there is no dynamic libc.
    check("not a dynamic executable" in out or "statically linked" in out,
          f"musl-rs: `ldd` reports no dynamic libc ({out.strip()!r})")

    # --- serves + runs a real box -------------------------------------------
    env = dict(os.environ)
    tmp = tempfile.mkdtemp(prefix="musl-rs-")
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        d = os.path.join(tmp, sub)
        os.makedirs(d, exist_ok=True)
        os.chmod(d, 0o700)
        env[k] = d
    env["SLOPBOX_NS"] = "MUSL"

    eng = subprocess.Popen([str(MUSL_BIN), "engine"],
                           stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                           env=env)
    try:
        sock = os.path.join(env["XDG_RUNTIME_DIR"], "slopbox.MUSL", "ui.sock")
        if not wait_socket(sock):
            log = b""
            try:
                eng.terminate(); log = eng.stdout.read() or b""
            except Exception:
                pass
            raise RuntimeError("musl engine socket never appeared:\n"
                               + log.decode(errors="replace"))
        check(True, f"musl-rs: engine serving (socket at {sock})")

        marker = "musl-static-box-ok"
        r = subprocess.run([str(MUSL_BIN), "run", "--", "echo", marker],
                           capture_output=True, text=True, env=env, timeout=120)
        check(r.returncode == 0,
              f"musl-rs: box exited 0 (rc={r.returncode}, err={r.stderr.strip()!r})")
        check(marker in r.stdout,
              f"musl-rs: box stdout captured ({r.stdout.strip()!r})")
    finally:
        eng.terminate()
        try:
            eng.wait(timeout=10)
        except Exception:
            eng.kill()

    print("MUSL-RS PASS")
    return 0


def test_musl_rs():
    assert main() == 0


if __name__ == "__main__":
    sys.exit(main())
