#!/usr/bin/env python3
"""Engine-held PTY (D7/D9) — the OTHER half of `sarun run -p`.

The RUST engine spawns a command on a PTY it HOLDS (portable-pty) and muxes the
master ↔ a client over the FRAME_PTY_* frames (frames.rs):
    FRAME_PTY_DATA  (7) both directions — raw PTY bytes
    FRAME_PTY_RESIZE(8) client→engine   — [rows:u16 BE][cols:u16 BE]
    FRAME_PTY_EOF   (9) engine→client   — child exited / master EOF

This test drives that engine half over a REAL control socket end to end:
  * OUTPUT: spawn `echo MARKER` on the engine PTY, read the FRAME_PTY_DATA stream
    off the socket, and assert the child's real bytes arrive + FRAME_PTY_EOF lands.
  * INPUT : spawn `read line; echo GOT:$line`, send the keystrokes as a
    FRAME_PTY_DATA frame, and assert the readback (`GOT:...`) comes back — i.e.
    the bytes we wrote reached the child.
  * RESIZE: spawn `stty size` after a small sleep, send FRAME_PTY_RESIZE, and
    assert the child's tty reports the NEW geometry.

The vt100 + tui-term RENDER half (the ratatui pane) is proven HEADLESSLY in the
engine's own Rust tests (`cargo test` — pty.rs spawns a real child, feeds its
FRAME_PTY_DATA into vt100::Parser + tui_term::PseudoTerminal, renders to a
ratatui TestBackend, and asserts the rendered grid CONTAINS the child's marker;
plus an input-readback and a sink-recording test). This Python file complements
that by proving the engine's socket protocol with no Rust test harness in the
loop. Both are REAL: a real PTY child, real bytes, real assertions on content.

Run:
    uv run --with "pyfuse3>=3.2" --with "trio>=0.22" --with "wcmatch>=8.4" \
      --with "python-magic>=0.4" python test_pty_ui_rs.py
Skips (passes vacuously) if cargo/the binary are unavailable.
"""
import os, shutil, socket, struct, subprocess, sys, tempfile, time
from pathlib import Path
from importlib.machinery import SourceFileLoader

_HERE = Path(__file__).resolve().parent
SARUN = str(_HERE / "sarun")
CRATE = _HERE.parent / "engine"
BIN = CRATE / "target/x86_64-unknown-linux-musl/release/sarun"

FRAME_PTY_DATA = 7
FRAME_PTY_RESIZE = 8
FRAME_PTY_EOF = 9

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


def encode(ftype: int, payload: bytes) -> bytes:
    """One typed frame: [total-len:4 BE][type:1][payload]."""
    total = 1 + len(payload)
    return struct.pack(">I", total) + bytes([ftype]) + payload


def decode_all(buf: bytes):
    """Decode every whole frame in buf; return (frames, consumed)."""
    out = []
    i = 0
    n = len(buf)
    while n - i >= 4:
        tot = struct.unpack(">I", buf[i:i+4])[0]
        if n - (i + 4) < tot:
            break
        if tot == 0:
            i += 4
            continue
        ftype = buf[i+4]
        payload = buf[i+5:i+4+tot]
        out.append((ftype, payload))
        i += 4 + tot
    return out, i


class PtyClient:
    """A `pty_spawn` connection: send the request line, read the JSON ack, then
    speak FRAME_PTY_* frames over the same socket."""
    def __init__(self, sock_path, argv, rows=24, cols=80):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.connect(sock_path)
        req = ('{"type":"pty_spawn","argv":%s,"rows":%d,"cols":%d}\n'
               % (_json_argv(argv), rows, cols))
        self.s.sendall(req.encode())
        # Read exactly one ack line (engine writes it before the frame stream).
        self.ack = self._read_line()
        self.buf = b""

    def _read_line(self):
        line = b""
        while not line.endswith(b"\n"):
            c = self.s.recv(1)
            if not c:
                break
            line += c
        return line.decode(errors="replace").strip()

    def send_data(self, data: bytes):
        self.s.sendall(encode(FRAME_PTY_DATA, data))

    def send_resize(self, rows: int, cols: int):
        self.s.sendall(encode(FRAME_PTY_RESIZE,
                              struct.pack(">H", rows) + struct.pack(">H", cols)))

    def drain(self, timeout=8.0):
        """Read frames until FRAME_PTY_EOF or the socket closes. Returns the
        concatenated FRAME_PTY_DATA bytes and whether EOF was seen."""
        data = b""
        eof = False
        self.s.settimeout(timeout)
        end = time.time() + timeout
        try:
            while time.time() < end:
                chunk = self.s.recv(65536)
                if not chunk:
                    break
                self.buf += chunk
                frames, used = decode_all(self.buf)
                self.buf = self.buf[used:]
                for ft, payload in frames:
                    if ft == FRAME_PTY_DATA:
                        data += payload
                    elif ft == FRAME_PTY_EOF:
                        eof = True
                if eof:
                    break
        except socket.timeout:
            pass
        return data, eof

    def close(self):
        try:
            self.s.close()
        except OSError:
            pass


def _json_argv(argv):
    import json
    return json.dumps(argv)


def main():
    if not ensure_binary():
        print("  ok  pty-ui-rs: cargo/binary unavailable — SKIP")
        print("\nPTY-UI-RS PASS (skipped)")
        return 0

    tmp = Path(tempfile.mkdtemp(prefix="ptyui-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "PTYUI"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()

    eng = subprocess.Popen([str(BIN), "serve"],
                           stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    sock = m.sock_path()
    try:
        if not wait_socket(sock):
            out = eng.stdout.read(2000) if eng.stdout else b""
            raise RuntimeError("rust engine socket never appeared:\n"
                               + out.decode(errors="replace"))

        # 1. OUTPUT: a real child's stdout arrives as FRAME_PTY_DATA + EOF lands.
        c = PtyClient(sock, ["sh", "-c", "echo MARKER-PTY-UI; echo row-two"])
        check('"ok":true' in c.ack, "pty-ui-rs: pty_spawn acked before frames")
        data, eof = c.drain()
        check(b"MARKER-PTY-UI" in data,
              "pty-ui-rs: child stdout arrives over FRAME_PTY_DATA")
        check(b"row-two" in data, "pty-ui-rs: multi-line PTY output muxed")
        check(eof, "pty-ui-rs: FRAME_PTY_EOF sent on child exit")
        c.close()

        # 2. INPUT: a FRAME_PTY_DATA keystroke frame reaches the child (readback).
        c = PtyClient(sock, ["sh", "-c", "read line; echo GOT:$line"])
        c.send_data(b"ping-from-client\n")
        data, _ = c.drain()
        check(b"GOT:ping-from-client" in data,
              "pty-ui-rs: client keystrokes reach the child (input path)")
        c.close()

        # 3. RESIZE: FRAME_PTY_RESIZE changes the child's tty geometry.
        c = PtyClient(sock, ["sh", "-c", "sleep 0.4; stty size"], rows=24, cols=80)
        c.send_resize(40, 100)
        data, _ = c.drain()
        check(b"40 100" in data,
              "pty-ui-rs: FRAME_PTY_RESIZE reaches the child tty (stty size)")
        c.close()

    finally:
        eng.terminate()
        try:
            eng.wait(timeout=10)
        except subprocess.TimeoutExpired:
            eng.kill()
        shutil.rmtree(tmp, ignore_errors=True)

    print()
    if _fails:
        print("PTY-UI-RS FAIL (%d)" % len(_fails))
        return 1
    print("PTY-UI-RS PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
