#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "textual>=0.60", "wcmatch>=8.4",
#   "pyfuse3>=3.2", "trio>=0.22", "python-magic>=0.4",
#   "openai>=1.30",
# ]
# ///
"""End-to-end: oaita's shell tool against a REAL sarun box.

Starts the real headless sarun UI, points oaita at the in-process fake OpenAI
server (an expect script plays the model), and lets `oaita run` drive a real
delegated shell execution: the script runs inside a real bwrap+FUSE box, its
output is captured, its file write is STAGED in the box (the host is never
touched), and the change summary lands in the conversation's `.tool` turn.

This is the full stack: expect-script → gen → c-flagged call turn →
evaluate_call → SarunExecutor → `sarun BOX -- sh -c …` → real overlay →
`sarun BOX patch` → result turn → synthesis gen.

    ./test_oaita_e2e.py        # needs bwrap+FUSE (work in this sandbox);
                               # first run builds patched pyfuse3 (~25 s, once)

Deliberately NOT pytest-collected (functions are e2e_*, like test_e2e.py):
it needs a real UI + box and takes a minute; the quick battery stays fast.
Self-safety: isolated XDG temp tree; the UI is killed and the box discarded
in a finally.
"""
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import socket as socket_mod
from importlib.machinery import SourceFileLoader
from pathlib import Path

HERE = Path(__file__).resolve().parent
PYBIN = sys.executable          # uv-equipped: sarun's deps + openai
SARUN = str(HERE / "sarun")

_fails = []


def check(msg, cond):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond:
        _fails.append(msg)


def wait_socket(sock, timeout=60):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket_mod.socket(socket_mod.AF_UNIX,
                                   socket_mod.SOCK_STREAM) as s:
                s.settimeout(1.0)
                s.connect(sock)
                return True
        except OSError:
            time.sleep(0.2)
    return False


def e2e_shell_in_real_box():
    tmp = Path(tempfile.mkdtemp(prefix="oaita-e2e-"))
    # The isolated XDG tree is shared by the UI subprocess, the box runner
    # children AND in-process oaita (session folders live under it too).
    os.environ["XDG_STATE_HOME"] = str(tmp / "state")
    os.environ["XDG_RUNTIME_DIR"] = str(tmp / "run")
    os.environ["XDG_CONFIG_HOME"] = str(tmp / "config")
    os.environ["XDG_DATA_HOME"] = str(tmp / "data")
    os.environ["OAITA_ID_SEED"] = "e2e"
    os.environ.setdefault("TERM", "dumb")
    os.environ["TEXTUAL"] = ""

    # Late imports: oaita + fakeserver under the env above; loading sarun in
    # the UI subprocess (not here) keeps this process FUSE-free.
    oaita = SourceFileLoader("oaita", str(HERE / "oaita")).load_module()
    fakeserver = SourceFileLoader(
        "oaita_fakeserver", str(HERE / "oaita_fakeserver")).load_module()

    sock = str(Path(os.environ["XDG_RUNTIME_DIR"]) / "slopbox" / "ui.sock")
    harness = tmp / "ui_harness.py"
    harness.write_text(
        "from importlib.machinery import SourceFileLoader\n"
        f"m = SourceFileLoader('slopbox', {SARUN!r}).load_module()\n"
        "m.ensure_dirs()\n"
        "app = m._make_ui_app()()\n"
        "app.run(headless=True)\n")
    ui = subprocess.Popen([PYBIN, str(harness)],
                          stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    srv = fakeserver.FakeOpenAIServer().start()
    executor = oaita.SarunExecutor([PYBIN, SARUN])
    try:
        check("real sarun UI came up (control socket answers)",
              wait_socket(sock, 90))

        marker = "OAITA-E2E-MARKER-7391"
        proof = "/oaita-e2e-proof.txt"
        oaita.add_turn("e2e", source=__import__("io").BytesIO(
            b"prove you can touch the sandbox"))
        srv.expect("prove you can touch the sandbox",
                   fakeserver.CannedChat(tool_calls=[(
                       "shell",
                       json.dumps({"script":
                                   f"echo {marker} && echo staged > {proof}"}),
                       "c1")]))
        srv.expect(marker,
                   fakeserver.CannedChat(
                       content="Done: marker echoed, file staged in the box."))

        produced = oaita.run_to_completion(
            "e2e", model="test-model", base_url=srv.base_url,
            api_key="k", echo=lambda _t: None, executor=executor,
            max_steps=6)

        turns = oaita.load_turns("e2e")
        check("arc settled: user, shell call, result, answer",
              [t.type for t in turns] ==
              ["user", "assistant", "tool", "assistant"])
        result = turns[2].read()
        check("REAL box output captured into the .tool turn",
              marker in result)
        check("run exited 0 through the real runner",
              result.startswith("exit 0"))
        check("the staged write shows in the change summary",
              "oaita-e2e-proof.txt: +1 -0" in result)
        check("the HOST was never touched (write stayed in the box)",
              not Path(proof).exists())
        check("synthesis reacted to the real result",
              turns[3].read() == "Done: marker echoed, file staged in the box.")
        check("run produced call, result, answer", len(produced) == 3)

        # The box is a first-class sarun citizen: its patch is queryable.
        patch = subprocess.run([PYBIN, SARUN, "OAITA-E2E", "patch"],
                               capture_output=True, text=True, timeout=60)
        check("`sarun OAITA-E2E patch` returns the staged diff",
              patch.returncode == 0 and "+staged" in patch.stdout)
    finally:
        subprocess.run([PYBIN, SARUN, "OAITA-E2E", "discard"],
                       capture_output=True, timeout=60)
        srv.stop()
        ui.terminate()
        try:
            ui.wait(timeout=15)
        except subprocess.TimeoutExpired:
            ui.kill()
        subprocess.run(["umount", "-l",
                        str(Path(os.environ["XDG_RUNTIME_DIR"])
                            / "slopbox" / "mnt")],
                       capture_output=True)
        shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    e2e_shell_in_real_box()
    print("\n" + ("E2E PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    sys.exit(1 if _fails else 0)
