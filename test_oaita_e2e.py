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

        # And inspect's box: locators reach those innards natively — the
        # staged change set is the one thing a shell cannot show.
        over = oaita.inspect_path("box:e2e", executor=executor)
        check("inspect box:<id> lists the REAL staged change set",
              "oaita-e2e-proof.txt: +1 -0" in over)
        rel = oaita._patch_files(executor.patch_text("OAITA-E2E"))[0][0]
        drill = oaita.inspect_path(f"box:e2e/{rel}", executor=executor)
        check("inspect box:<id>/<file> pages the staged diff itself",
              "+staged" in drill and "staged diff, +1 -0" in drill)
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


def e2e_act_in_real_box():
    """act's full path against a REAL box: the sub-agent is `oaita run` running
    INSIDE a sarun box, reaching the fake server over the host netns, its answer
    read back through `oaita tail`-in-box. Proves env propagation into the box,
    that the sub-agent's writes STAGE (host sees only the seed until apply), and
    that applying the box folds them up."""
    # NOTE: under /var/tmp, not /tmp — the box masks /tmp with a fresh tmpfs,
    # so a state tree there would be invisible to the in-box sub-agent. A real
    # $XDG_STATE_HOME (~/.local/state, under $HOME) is likewise un-masked, so
    # this mirrors real use rather than working around it.
    tmp = Path(tempfile.mkdtemp(prefix="oaita-e2e-act-", dir="/var/tmp"))
    os.environ["XDG_STATE_HOME"] = str(tmp / "state")
    os.environ["XDG_RUNTIME_DIR"] = str(tmp / "run")
    os.environ["XDG_CONFIG_HOME"] = str(tmp / "config")
    os.environ["XDG_DATA_HOME"] = str(tmp / "data")
    os.environ["OAITA_ID_SEED"] = "e2e-act"
    os.environ.setdefault("TERM", "dumb")
    os.environ["TEXTUAL"] = ""

    oaita = SourceFileLoader("oaita", str(HERE / "oaita")).load_module()
    fakeserver = SourceFileLoader(
        "oaita_fakeserver", str(HERE / "oaita_fakeserver")).load_module()

    # The sub-agent subprocess (inside the box) reads ALL of this from the
    # environment — which bwrap inherits (no --clearenv on the box runner).
    srv = fakeserver.FakeOpenAIServer().start()
    os.environ["OPENAI_BASE_URL"] = srv.base_url
    os.environ["OPENAI_API_KEY"] = "k"
    os.environ["OAITA_MODEL"] = "test-model"
    os.environ["OAITA_CMD"] = f"{PYBIN} {HERE / 'oaita'}"   # skip per-box uv

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
    executor = oaita.SarunExecutor([PYBIN, SARUN])
    box = None
    try:
        check("act: real sarun UI came up", wait_socket(sock, 90))

        oaita.add_turn("main", source=__import__("io").BytesIO(
            b"delegate: greet the sandbox"))
        greeting = "HELLO FROM INSIDE THE BOX"
        srv.expect("delegate: greet the sandbox",
                   fakeserver.CannedChat(tool_calls=[(
                       "act", json.dumps({"request": "say a greeting"}),
                       "call_greeter")]))
        srv.expect("say a greeting",            # the sub-agent, in its box
                   fakeserver.CannedChat(content=greeting))
        srv.expect(greeting,                    # main reacting to the result
                   fakeserver.CannedChat(content="Greeting delegated and received."))

        produced = oaita.run_to_completion(
            "main", model="test-model", base_url=srv.base_url, api_key="k",
            echo=lambda _t: None, executor=executor, max_steps=8)

        main = oaita.load_turns("main")
        check("act: main settled — user, call, result, answer",
              [t.type for t in main] ==
              ["user", "assistant", "tool", "assistant"])
        innerid = main[1].slug
        box = oaita.box_name(innerid)
        check("act: the .tool result carries the sub-agent's in-box answer",
              greeting in main[2].read())
        check("act: result's from-sender is the sub-agent session",
              main[2].sender == innerid)
        check("act: main reacted to the delegated greeting",
              main[3].read() == "Greeting delegated and received.")
        check("act: run produced call, result, answer", len(produced) == 3)

        # The gate: the sub-agent's reply was STAGED in its box — on the HOST
        # the inner session still holds ONLY the seed the parent wrote.
        host_inner = oaita.load_turns(innerid)
        check("act: host sees ONLY the seed (sub-agent writes are staged)",
              [t.type for t in host_inner] == ["user"]
              and host_inner[0].read() == "say a greeting")

        # And the box's patch shows the staged session writes as added files.
        patch = subprocess.run([PYBIN, SARUN, box, "patch"],
                               capture_output=True, text=True, timeout=60)
        check("act: the box patch shows the sub-agent's staged turn file(s)",
              patch.returncode == 0
              and f"/oaita/{innerid}/" in patch.stdout
              and greeting in patch.stdout)

        # Apply the box → the sub-agent's turns fold up onto the host.
        executor.apply_box(box)
        box = None                              # consumed by apply
        folded = oaita.load_turns(innerid)
        check("act: applying the box folds the sub-agent's answer onto the host",
              [t.type for t in folded] == ["user", "assistant"]
              and folded[-1].read() == greeting)
    finally:
        if box is not None:
            subprocess.run([PYBIN, SARUN, box, "discard"],
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
    e2e_act_in_real_box()
    print("\n" + ("E2E PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    sys.exit(1 if _fails else 0)
