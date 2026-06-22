#!/usr/bin/env python3
"""PTY mode (`sarun run -p`) for the RUST engine (engine/): an interactive
controlling-tty box whose output is ALSO captured into the outputs table.

REAL effects, driven through a real pty (Python's `pty.openpty()`), never
shape-only:
  - the child sees a TTY: `-p -- sh -c 'test -t 1 && echo PTYYES || echo PTYNO'`
    captures PTYYES; the SAME command WITHOUT -p captures PTYNO (negative
    control). Asserted by reading the box's captured outputs from the sqlar.
  - interactivity: `-p -- sh -c 'read x; echo got:$x'` + writing "hello\n" to
    the pty master → the captured output contains "got:hello".
  - the run exits cleanly under a bounded timeout (no hang); the runner's own
    terminal is restored (we run the harness itself on a pty slave and confirm
    its termios is unchanged after the box exits).

Run:
    uv run --with "pyfuse3>=3.2" --with "trio>=0.22" --with "wcmatch>=8.4" \
      --with "python-magic>=0.4" python test_pty_rs.py
Skips (passes vacuously) if cargo/the binary/FUSE are unavailable.
"""
import os, pty, select, shutil, socket, subprocess, sys, tempfile, termios, time
from pathlib import Path
from importlib.machinery import SourceFileLoader

_HERE = Path(__file__).resolve().parent
SARUN = str(_HERE / "sarun")
CRATE = _HERE.parent / "engine"
BIN = CRATE / "target/x86_64-unknown-linux-musl/release/sarun"

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


def fuse_ok() -> bool:
    return Path("/dev/fuse").exists() and shutil.which("bwrap") is not None


def wait_socket(sock, timeout=30):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.settimeout(1.0); s.connect(sock); return True
        except OSError:
            time.sleep(0.1)
    return False


def latest_sid(m):
    sps = list(Path(os.environ["XDG_STATE_HOME"]).joinpath("slopbox.RS").glob("*.sqlar"))
    if not sps:
        return None
    return max(sps, key=lambda p: int(p.stem)).stem


def captured_bytes(m, sid) -> bytes:
    """All recorded output bytes for the at-rest box, concatenated."""
    sp = m.sqlar_path(sid)
    out = b""
    for row in m.outputs_list(sp):
        d = m.outputs_get(sp, row["id"])
        if d and d.get("content"):
            out += d["content"]
    return out


def run_on_pty(argv, feed=None, timeout=60):
    """Run `argv` with the child's stdin/stdout on a fresh pty slave (so the
    RUST runner sees a real tty on its fds, taking the PTY path). Optionally
    feed bytes to the master after a short delay. Returns (exit_code, master_out,
    termios_unchanged)."""
    mfd, sfd = pty.openpty()
    pre = termios.tcgetattr(mfd)
    p = subprocess.Popen(argv, stdin=sfd, stdout=sfd, stderr=sfd,
                         start_new_session=True)
    os.close(sfd)
    collected = b""
    fed = False
    start = time.time()
    deadline = start + timeout
    while True:
        # Feed the line ~0.5s in: by then the box is up and the child is
        # blocked in read() on its pty slave.
        if feed is not None and not fed and time.time() - start > 0.5:
            os.write(mfd, feed); fed = True
        if p.poll() is not None and not _drainable(mfd):
            break
        if time.time() > deadline:
            p.kill(); raise RuntimeError("PTY run timed out (possible hang)")
        r, _, _ = select.select([mfd], [], [], 0.2)
        if mfd in r:
            try:
                chunk = os.read(mfd, 65536)
            except OSError:
                break
            if not chunk:
                break
            collected += chunk
    try:
        code = p.wait(timeout=10)
    except subprocess.TimeoutExpired:
        p.kill(); code = -9
    # master termios unchanged after the box exits == runner restored its tty.
    try:
        post = termios.tcgetattr(mfd)
        unchanged = (post == pre)
    except OSError:
        unchanged = True
    os.close(mfd)
    return code, collected, unchanged


def _drainable(fd):
    r, _, _ = select.select([fd], [], [], 0)
    return bool(r)


def main():
    if not ensure_binary():
        raise SystemExit("test_pty_rs: engine binary unavailable — run `make engine`")
    if not fuse_ok():
        raise SystemExit("test_pty_rs: FUSE/bwrap unavailable")
    tmp = Path(tempfile.mkdtemp(prefix="ptyrs-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "RS"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    eng = None
    try:
        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        sock = m.sock_path()
        if not wait_socket(sock):
            raise RuntimeError("rust engine socket never appeared")

        # ── the child sees a TTY (PTY path) ─────────────────────────────────
        code, out, restored = run_on_pty(
            [str(BIN), "run", "-p", "--", "sh", "-c",
             "test -t 1 && echo PTYYES || echo PTYNO"])
        check(code == 0, "pty-rs: -p run exits 0 (no hang)")
        sid = latest_sid(m)
        cap = captured_bytes(m, sid) if sid else b""
        check(b"PTYYES" in cap,
              f"pty-rs: child saw a TTY, captured PTYYES (cap={cap[:40]!r})")
        check(b"PTYNO" not in cap, "pty-rs: child did NOT report a non-tty")
        check(restored, "pty-rs: runner restored the controlling terminal's termios")

        # ── negative control: same command WITHOUT -p captures PTYNO ─────────
        code2, _, _ = run_on_pty(
            [str(BIN), "run", "--", "sh", "-c",
             "test -t 1 && echo PTYYES || echo PTYNO"])
        check(code2 == 0, "pty-rs: non-pty run exits 0")
        sid2 = latest_sid(m)
        cap2 = captured_bytes(m, sid2) if sid2 else b""
        check(b"PTYNO" in cap2 and b"PTYYES" not in cap2,
              f"pty-rs: NEGATIVE control — no -p captured PTYNO (cap={cap2[:40]!r})")

        # ── -p with NON-tty runner stdio STILL gives the child a pty ─────────
        # The whole point of -p is "the child runs on a pty" (like script /
        # docker -t): the runner having no tty must NOT silently downgrade to a
        # headless box. Run with plain pipes (no pty for the runner) and assert
        # the child still saw a tty + the run didn't hang.
        rno = subprocess.run(
            [str(BIN), "run", "-p", "--", "sh", "-c",
             "test -t 1 && echo PTYYES || echo PTYNO"],
            stdin=subprocess.DEVNULL, capture_output=True, timeout=60)
        check(rno.returncode == 0, "pty-rs: -p with non-tty stdio exits 0 (no hang)")
        sidn = latest_sid(m)
        capn = captured_bytes(m, sidn) if sidn else b""
        check(b"PTYYES" in capn and b"PTYNO" not in capn,
              f"pty-rs: -p gave the child a pty even with NON-tty runner stdio "
              f"(cap={capn[:40]!r})")

        # ── interactivity: feed a line into the master, child reads it ───────
        code3, out3, _ = run_on_pty(
            [str(BIN), "run", "-p", "--", "sh", "-c", "read x; echo got:$x"],
            feed=b"hello\n")
        check(code3 == 0, "pty-rs: interactive -p run exits 0")
        sid3 = latest_sid(m)
        cap3 = captured_bytes(m, sid3) if sid3 else b""
        check(b"got:hello" in cap3,
              f"pty-rs: child READ the fed line, captured got:hello "
              f"(cap={cap3[:60]!r})")
    finally:
        if eng is not None and eng.poll() is None:
            eng.terminate()
            try: eng.wait(timeout=10)
            except Exception: eng.kill()
        os.environ.pop("SLOPBOX_NS", None)
        shutil.rmtree(tmp, ignore_errors=True)
    print("\n" + ("PTY-RS PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_pty_rs():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
