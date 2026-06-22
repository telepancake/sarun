#!/usr/bin/env python3
"""oaita end-to-end against a stub OpenAI endpoint (no external network).

Exercises the real path with no mocks of our own code: oaita.toml parse ->
HTTP client -> SSE stream decode -> turn-file persistence. A local HTTP server
returns a canned chat-completion SSE stream; `oaita gen` must connect, decode
the deltas, and write the assistant turn to disk with the concatenated content.
`oaita tail` must then print it, and `oaita where` must report the sandboxed
config path.

Drives the Rust binary directly (no Python prototype dependency). Run:
    uv run --with pytest python -m pytest test_oaita_rs.py
"""
import http.server
import json
import os
import socket
import subprocess
import tempfile
import threading
from pathlib import Path

_HERE = Path(__file__).resolve().parent
BIN = _HERE.parent / "engine/target/x86_64-unknown-linux-musl/release/sarun"

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond:
        _fails.append(msg)


REPLY_CHUNKS = ["Hello", ", ", "world"]
REPLY_TEXT = "".join(REPLY_CHUNKS)


class _StubHandler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_POST(self):
        n = int(self.headers.get("Content-Length", "0"))
        body = json.loads(self.rfile.read(n) or b"{}")
        # Record what the client sent so the test can assert the request shape.
        self.server.last_request = body
        self.server.last_auth = self.headers.get("Authorization")
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        for c in REPLY_CHUNKS:
            frame = json.dumps({"choices": [{"delta": {"content": c}}]})
            self.wfile.write(f"data: {frame}\n\n".encode())
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()


def _free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def _oaita(env, *args, stdin=None):
    return subprocess.run([str(BIN), "oaita", *args], env=env, input=stdin,
                          capture_output=True, text=True)


def run():
    assert BIN.exists(), f"engine binary missing: {BIN} — run `make engine`"

    port = _free_port()
    srv = http.server.HTTPServer(("127.0.0.1", port), _StubHandler)
    srv.last_request = None
    srv.last_auth = None
    t = threading.Thread(target=srv.serve_forever, daemon=True)
    t.start()
    try:
        tmp = Path(tempfile.mkdtemp(prefix="oaita-"))
        env = dict(os.environ)
        for k, sub in (("XDG_CONFIG_HOME", "config"), ("XDG_STATE_HOME", "state"),
                       ("XDG_DATA_HOME", "data"), ("XDG_RUNTIME_DIR", "run")):
            d = tmp / sub
            d.mkdir(parents=True, exist_ok=True)
            env[k] = str(d)
        env.pop("SLOPBOX_NS", None)

        cfg_dir = tmp / "config/slopbox"
        cfg_dir.mkdir(parents=True, exist_ok=True)
        (cfg_dir / "oaita.toml").write_text(
            f'model = "stub-model"\n'
            f'base_url = "http://127.0.0.1:{port}/v1"\n'
            f'api_key = "sk-test-123"\n')

        # `where` reports the sandboxed config path and the parsed model.
        r = _oaita(env, "where")
        check(r.returncode == 0, "where exits 0")
        check(str(cfg_dir / "oaita.toml") in r.stdout, "where reports the config path")
        check("stub-model" in r.stdout, "where reports the configured model")

        # Seed a user turn, then generate the assistant reply from the stub.
        r = _oaita(env, "add", "chat", "--type", "user", stdin="Say hi")
        check(r.returncode == 0, f"add user turn exits 0 ({r.stderr.strip()})")

        r = _oaita(env, "gen", "chat")
        check(r.returncode == 0, f"gen exits 0 ({r.stderr.strip()})")

        sess = tmp / "state/slopbox/oaita/chat"
        turns = sorted(sess.glob("*")) if sess.exists() else []
        assistant = [p for p in turns if p.name.endswith(".assistant")]
        check(len(assistant) == 1, f"exactly one assistant turn written ({[p.name for p in turns]})")
        if assistant:
            content = assistant[0].read_text()
            check(content == REPLY_TEXT,
                  f"assistant turn holds the streamed text (got {content!r})")

        # The client actually reached the stub with the configured credentials.
        check(srv.last_auth == "Bearer sk-test-123", "client sent the Bearer key")
        check(srv.last_request is not None and srv.last_request.get("model") == "stub-model",
              "client sent the configured model")
        check(bool(srv.last_request) and srv.last_request.get("stream") is True,
              "client requested streaming")

        # `tail` prints the settled assistant answer.
        r = _oaita(env, "tail", "chat")
        check(r.returncode == 0, "tail exits 0")
        check(REPLY_TEXT in r.stdout, "tail prints the assistant answer")
    finally:
        srv.shutdown()


def test_oaita():
    run()
    assert not _fails, "; ".join(_fails)


if __name__ == "__main__":
    import sys
    run()
    print("\nOAITA-RS", "FAIL" if _fails else "PASS")
    sys.exit(1 if _fails else 0)
