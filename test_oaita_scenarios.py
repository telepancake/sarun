#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "openai>=1.30",
#   "pytest>=8",
# ]
# ///
"""Scripted end-to-end scenarios for oaita — the expect-style harness in anger.

Each scenario is an "expect script": the fake server is loaded with
match-and-respond rules (`srv.expect`) that parrot back predefined model turns
keyed on REQUEST CONTENT (not arrival order), oaita is driven through its real
gen/call/run flows, and the assertions read the session FOLDERS — filenames,
flags, from-senders, raw contents — plus the captured wire traffic.

$OAITA_ID_SEED makes every generated turn-id a hash of its (session, NNNN)
slot, so a scenario's folder state is bit-for-bit reproducible (scenario 5
literally runs the same script twice in two fresh state homes and compares the
trees byte-for-byte).

Scenarios:
  1. Research delegation, full arc (run): user asks → model delegates via act
     → sub-agent answers → model synthesizes. Asserts the c-flagged envelope
     turn, the from=<outer> seed, the from=<inner> result, tool advertisement
     on the outer gens, NO tools on the leaf, and the exact final shape.
  2. Sender-targeted follow-up (run): a second round follows up the SAME
     sub-agent via follow_up=<its id> (the from-address of round 1's result).
     Asserts inner-session continuity (no duplicate session) and that the new
     result's sender is the followed session, not the new call's id.
  3. Interrupted-stream resume (gen): a p-flagged partial assistant tail is
     excluded from the prompt and regenerated in place; the p flag drops.
  4. Parallel fan-out (run): one gen emits TWO act calls; `call` evaluates
     them one per step with positional pairing; results land with the right
     senders; the final synthesis sees both.
  5. Determinism replay: scenario 1 re-run from scratch in a second state home
     produces the IDENTICAL tree (filenames and bytes).
  6. CLI subprocess: the same delegation arc driven through the real
     `oaita run` command line (env-configured), proving the installed UX.

Dual style: standalone (`./test_oaita_scenarios.py` → `ALL PASS`) and pytest.
"""
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from importlib.machinery import SourceFileLoader
from pathlib import Path

HERE = Path(__file__).resolve().parent


def _load(name: str):
    """Load an extensionless module ONCE per process (see test_oaita._load)."""
    if name in sys.modules:
        return sys.modules[name]
    return SourceFileLoader(name, str(HERE / name)).load_module()


oaita = _load("oaita")
fakeserver = _load("oaita_fakeserver")

CannedChat = fakeserver.CannedChat

_fails = []


def check(msg, cond):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond:
        _fails.append(msg)


# ── fixtures ─────────────────────────────────────────────────────────────────
class Stage:
    """A throwaway $XDG_STATE_HOME + live fake server + pinned id seed.

    Also exports the model/server config and $OAITA_EXECUTOR=local into the
    environment, because act sub-agents are real `oaita run` SUBPROCESSES now
    (LocalExecutor: ungated stand-in for a sarun box) — they read everything
    from env and answer against the same fake server."""

    _ENV = ("XDG_STATE_HOME", "OAITA_ID_SEED", "OPENAI_BASE_URL",
            "OPENAI_API_KEY", "OAITA_MODEL", "OAITA_EXECUTOR", "OAITA_CMD")

    def __init__(self, seed="scenario"):
        self.tmp = Path(tempfile.mkdtemp(prefix="oaita-scen-"))
        self._prev = {k: os.environ.get(k) for k in self._ENV}
        self.srv = fakeserver.FakeOpenAIServer().start()
        os.environ.update(
            XDG_STATE_HOME=str(self.tmp / "state"),
            OAITA_ID_SEED=seed,
            OPENAI_BASE_URL=self.srv.base_url,
            OPENAI_API_KEY="test-key",
            OAITA_MODEL="test-model",
            OAITA_EXECUTOR="local",
            # Sub-processes reuse THIS deps-equipped interpreter (no uv spawn).
            OAITA_CMD=f"{sys.executable} {HERE / 'oaita'}",
        )

    def close(self):
        try:
            self.srv.stop()
        finally:
            for key, prev in self._prev.items():
                if prev is None:
                    os.environ.pop(key, None)
                else:
                    os.environ[key] = prev
            shutil.rmtree(self.tmp, ignore_errors=True)

    def write_turn(self, name, filename, content):
        folder = oaita.session_dir(name)
        folder.mkdir(parents=True, exist_ok=True)
        (folder / filename).write_text(content, encoding="utf-8")

    def _kw(self):
        return dict(model="test-model", base_url=self.srv.base_url,
                    api_key="test-key", echo=lambda _t: None)

    def gen(self, name, **kw):
        return oaita.generate(name, **self._kw(), **kw)

    def run(self, name, **kw):
        return oaita.run_to_completion(name, **self._kw(), **kw)


def turns_of(name):
    """Parsed turns of a session, in order."""
    return oaita.load_turns(name)


def tree_snapshot(root: Path) -> dict:
    """{relative-path: content} for every file under root (sorted keys)."""
    out = {}
    for p in sorted(root.rglob("*")):
        if p.is_file():
            out[str(p.relative_to(root))] = p.read_text(encoding="utf-8")
    return out


def act_call(request, **extra):
    """A canned model turn that calls act with the given request (+fields)."""
    args = {"request": request, **extra}
    return CannedChat(tool_calls=[("act", json.dumps(args), "call_x")])


def _last(req):
    """The content of the last message in a captured request ('' if none)."""
    msgs = req.messages or []
    c = msgs[-1].get("content") if msgs else None
    return c if isinstance(c, str) else ""


def _has_tool(req, name):
    """True if any message in the request is a native tool_call for `name`."""
    for m in req.messages or []:
        for tc in m.get("tool_calls") or []:
            if tc.get("function", {}).get("name") == name:
                return True
    return False


# ── scenario 1+2: delegation arc, then sender-targeted follow-up ─────────────
def _script_delegation(srv):
    """The expect script for the round-1 delegation arc (used twice: by the
    in-process scenario and by the determinism replay)."""
    srv.expect("delegate the license research",
               act_call("research MIT vs GPL differences",
                        data="context: a parser library"))
    srv.expect("research MIT vs GPL differences",
               CannedChat(content="MIT is permissive; GPL is copyleft."))
    srv.expect("MIT is permissive",
               CannedChat(content="Use MIT for the parser."))


def _play_delegation(st):
    st.write_turn("main", "0010.user",
                  "Compare MIT and GPL for our parser; "
                  "delegate the license research.")
    _script_delegation(st.srv)
    # LocalExecutor: the sub-agent is a REAL `oaita run` subprocess answering
    # against the same fake server (ungated stand-in for a sarun box).
    return st.run("main", executor=oaita.LocalExecutor())


def test_scenario_delegation_and_follow_up():
    st = Stage()
    try:
        produced = _play_delegation(st)

        # ── folder shape after the arc ──
        turns = turns_of("main")
        check("S1: main is user, call, result, answer",
              [t.type for t in turns] == ["user", "assistant", "tool",
                                          "assistant"])
        call_t, result_t, answer_t = turns[1], turns[2], turns[3]
        check("S1: the call turn is c-flagged", call_t.flags == "c")
        check("S1: call envelope holds tool + request + data",
              json.loads(call_t.read()) == {
                  "tool": "act",
                  "arguments": {"request": "research MIT vs GPL differences",
                                "data": "context: a parser library"}})
        check("S1: run produced call, result, answer", len(produced) == 3)
        check("S1: the answer is the synthesis, clean (no flags)",
              answer_t.read() == "Use MIT for the parser."
              and answer_t.flags == "")

        # ── the sub-agent ──
        innerid = result_t.sender
        check("S1: result's from-sender IS the call's turn-id",
              innerid == call_t.slug)
        inner = turns_of(innerid)
        check("S1: inner session is seed-user + answer",
              [t.type for t in inner] == ["user", "assistant"])
        check("S1: inner seed includes request and data",
              inner[0].read() ==
              "research MIT vs GPL differences\n\ncontext: a parser library")
        check("S1: inner seed's from-sender is the outer session",
              inner[0].sender == "main")

        # ── wire traffic ──
        reqs = st.srv.requests
        check("S1: exactly three model requests (gen, leaf, gen)",
              len(reqs) == 3)
        check("S1: every gen advertises the full registry (incl. the boxed "
              "sub-agent's own, from its subprocess)",
              all(r.tools and {t["function"]["name"] for t in r.tools}
                  == set(oaita.tool_registry())
                  for r in reqs))
        # The synthesis prompt narrates the call: assistant turn = envelope
        # JSON as content, tool turn with the from-bearing header.
        synth = reqs[2].messages
        check("S1: synthesis prompt is user, assistant, tool",
              [m["role"] for m in synth] == ["user", "assistant", "tool"])
        # The call replays in NATIVE tool-call form (send-time adapter): the
        # envelope rides in tool_calls; call/result messages carry NO injected
        # header (template-special regions stay pure payloads) — the turn-id
        # rides the native tool_call_id channel instead.
        tcs = synth[1].get("tool_calls")
        check("S1: call turn replays as native assistant.tool_calls",
              tcs and tcs[0]["function"]["name"] == "act"
              and json.loads(tcs[0]["function"]["arguments"])["request"]
              == "research MIT vs GPL differences"
              and tcs[0]["id"] == f"call_{call_t.slug}")
        check("S1: native call message content is empty (no header bait)",
              synth[1]["content"] is None)
        check("S1: result message carries the matching tool_call_id",
              synth[2].get("tool_call_id") == f"call_{call_t.slug}")
        check("S1: tool result payload is RAW (no injected header)",
              synth[2]["content"] == "MIT is permissive; GPL is copyleft.")
        check("S1: free-prose turns still carry their headers",
              synth[0]["content"].startswith('{"turn-id": "'))

        # ── round 2: follow up the SAME researcher by its from-address ──
        st.write_turn("main", "0050.user",
                      "Ask the researcher specifically about GPLv2 vs v3.")
        st.srv.expect("GPLv2 vs v3",
                      act_call("GPLv2 vs GPLv3?", follow_up=innerid))
        st.srv.expect("GPLv2 vs GPLv3?",
                      CannedChat(content="v3 adds patent clauses."))
        st.srv.expect("v3 adds patent clauses",
                      CannedChat(content="GPLv3 then."))
        st.run("main", executor=oaita.LocalExecutor())

        inner2 = turns_of(innerid)
        check("S2: the SAME inner session continued (4 turns, no duplicate)",
              [t.type for t in inner2] ==
              ["user", "assistant", "user", "assistant"])
        check("S2: follow-up question landed in the researcher session",
              inner2[2].read() == "GPLv2 vs GPLv3?"
              and inner2[2].sender == "main")
        turns2 = turns_of("main")
        check("S2: new result's sender is the FOLLOWED session, "
              "not the new call's id",
              turns2[-2].type == "tool" and turns2[-2].sender == innerid
              and turns2[-3].slug != innerid)
        check("S2: main settled on the round-2 answer",
              turns2[-1].read() == "GPLv3 then." and turns2[-1].flags == "")
        # Leaf request for round 2 saw the WHOLE researcher history.
        leaf2 = next(r for r in st.srv.requests
                     if r.messages and "GPLv2 vs GPLv3?"
                     in r.messages[-1]["content"])
        check("S2: leaf prompt carried the researcher's round-1 history",
              "MIT is permissive; GPL is copyleft." in
              "".join(m["content"] for m in leaf2.messages))
    finally:
        st.close()


# ── scenario 3: interrupted-stream resume ────────────────────────────────────
def test_scenario_partial_resume():
    st = Stage()
    try:
        st.write_turn("draft", "0010.user", "write a haiku about overlays")
        # An interrupt mid-stream left a p-flagged partial on disk.
        st.write_turn("draft", "0020-zzzzz.p.assistant", "Filesystems bloom")
        full = "Filesystems bloom\ncopy-on-write petals fall\nnothing lost below"
        # The matcher proves the partial was EXCLUDED from the prompt: it keys
        # on the user turn being the LAST message.
        st.srv.expect(
            lambda req: req.messages[-1]["content"].endswith(
                "write a haiku about overlays"),
            CannedChat(content=full))
        produced = st.gen("draft")
        turns = turns_of("draft")
        check("S3: still exactly two turns (regenerated in place)",
              len(turns) == 2)
        check("S3: the partial kept its id and dropped the p flag",
              produced[0].name == "0020-zzzzz.assistant")
        check("S3: content is the full regenerated haiku",
              produced[0].read_text(encoding="utf-8") == full)
        check("S3: exactly one request, with the partial excluded",
              len(st.srv.requests) == 1
              and len(st.srv.requests[0].messages) == 1)
    finally:
        st.close()


# ── scenario 4: parallel fan-out ─────────────────────────────────────────────
def test_scenario_parallel_fanout():
    st = Stage()
    try:
        st.write_turn("fan", "0010.user", "Check pyfuse3 and bwrap versions.")
        st.srv.expect("Check pyfuse3 and bwrap versions.", CannedChat(
            tool_calls=[
                ("act", '{"request": "pyfuse3 version?"}', "cA"),
                ("act", '{"request": "bwrap version?"}', "cB"),
            ]))
        st.srv.expect("pyfuse3 version?", CannedChat(content="pyfuse3 3.4.2"))
        st.srv.expect("bwrap version?", CannedChat(content="bwrap 0.8.0"))
        st.srv.expect("bwrap 0.8.0",
                      CannedChat(content="pyfuse3 3.4.2, bwrap 0.8.0."))
        st.run("fan", executor=oaita.LocalExecutor())

        turns = turns_of("fan")
        check("S4: shape is user, call, call, result, result, answer",
              [t.type for t in turns] ==
              ["user", "assistant", "assistant", "tool", "tool", "assistant"])
        callA, callB, resA, resB = turns[1], turns[2], turns[3], turns[4]
        check("S4: positional pairing — result k answers call k",
              resA.sender == callA.slug and resB.sender == callB.slug)
        check("S4: each result is its own sub-agent's answer",
              resA.read() == "pyfuse3 3.4.2" and resB.read() == "bwrap 0.8.0")
        check("S4: two isolated sub-agent sessions",
              turns_of(callA.slug)[0].read() == "pyfuse3 version?"
              and turns_of(callB.slug)[0].read() == "bwrap version?")
        check("S4: synthesis saw both results",
              turns[-1].read() == "pyfuse3 3.4.2, bwrap 0.8.0.")
        check("S4: four model requests (gen + 2 leaves + gen)",
              len(st.srv.requests) == 4)
    finally:
        st.close()


# ── scenario 5: determinism replay ───────────────────────────────────────────
def test_scenario_determinism_replay():
    def play():
        st = Stage(seed="replay")
        try:
            _play_delegation(st)
            return tree_snapshot(oaita.state_home())
        finally:
            st.close()

    t1, t2 = play(), play()
    check("S5: two from-scratch replays produce identical trees "
          f"({len(t1)} files)", t1 == t2 and len(t1) >= 6)
    # And the tree REALLY pins filenames: a different seed moves them.
    st = Stage(seed="otherseed")
    try:
        _play_delegation(st)
        t3 = tree_snapshot(oaita.state_home())
        check("S5: a different seed yields different filenames",
              set(t3) != set(t1))
    finally:
        st.close()


# ── scenario 6: the same arc through the real CLI ────────────────────────────
def test_scenario_cli_subprocess():
    st = Stage(seed="cli")
    try:
        env = dict(os.environ,
                   OPENAI_BASE_URL=st.srv.base_url,
                   OPENAI_API_KEY="test-key",
                   OAITA_MODEL="test-model")
        # Seed the session through the real `oaita add` (stdin → turn file).
        added = subprocess.run(
            [sys.executable, str(HERE / "oaita"), "add", "clidemo"],
            input="Compare MIT and GPL for our parser; "
                  "delegate the license research.",
            capture_output=True, text=True, timeout=60, env=env)
        check("S6: `oaita add` exits 0 and prints the turn path",
              added.returncode == 0
              and added.stdout.strip().endswith(".user"))
        added_path = Path(added.stdout.strip())
        check("S6: the added turn holds stdin verbatim",
              added_path.read_text(encoding="utf-8").endswith(
                  "delegate the license research."))
        _script_delegation(st.srv)
        proc = subprocess.run(
            [sys.executable, str(HERE / "oaita"), "run", "clidemo",
             "--max-steps", "8"],
            capture_output=True, text=True, timeout=120, env=env)
        check("S6: `oaita run` exits 0",
              proc.returncode == 0)
        check("S6: the final answer streamed to stdout",
              "Use MIT for the parser." in proc.stdout)
        check("S6: stderr reports each written turn",
              proc.stderr.count("oaita: wrote") == 3)
        turns = turns_of("clidemo")
        check("S6: folder settled exactly like the in-process arc",
              [t.type for t in turns] ==
              ["user", "assistant", "tool", "assistant"]
              and turns[-1].read() == "Use MIT for the parser.")
    finally:
        st.close()


# ── scenario 7: fix-the-build arc with the shell tool ────────────────────────
class FakeExecutor:
    """Scripted executor: canned ExecResults + a call log (mirrors the one in
    test_oaita; duplicated to keep each test file self-contained)."""

    def __init__(self):
        self.calls = []
        self.queue = []

    def stage(self, output, changes="", exit_code=0):
        self.queue.append(oaita.ExecResult(
            output=output, changes=changes, exit_code=exit_code))
        return self

    def run_script(self, script, box):
        self.calls.append((script, box))
        return self.queue.pop(0)


def test_scenario_shell_fix_build():
    st = Stage()
    try:
        st.write_turn("build", "0010.user", "the build is broken, fix it")
        st.srv.expect("the build is broken",
                      CannedChat(tool_calls=[
                          ("shell", '{"script": "make"}', "c1")]))
        # The model reads the failure, edits, re-runs — keyed on result text.
        st.srv.expect("missing semicolon",
                      CannedChat(tool_calls=[(
                          "shell",
                          '{"script": "sed -i \'7s/$/;/\' main.c && make"}',
                          "c2")]))
        st.srv.expect("main.c: +1 -1",
                      CannedChat(content="Fixed: line 7 lacked a semicolon. "
                                         "Build passes; the edit is staged "
                                         "in the box."))
        ex = (FakeExecutor()
              .stage("main.c:7: error: missing semicolon", exit_code=2)
              .stage("cc -o app main.c\nok",
                     changes="1 file(s) changed (staged in the box):\n"
                             "main.c: +1 -1"))
        st.run("build", executor=ex)

        turns = turns_of("build")
        check("S7: arc is user, shell, result, shell, result, answer",
              [t.type for t in turns] ==
              ["user", "assistant", "tool", "assistant", "tool", "assistant"])
        check("S7: both runs landed in the session's ONE persistent box",
              [c[1] for c in ex.calls] == ["OAITA-BUILD", "OAITA-BUILD"])
        check("S7: the second script reacted to the first failure",
              "sed -i" in ex.calls[1][0])
        check("S7: failure result carried the compiler error",
              "missing semicolon" in turns[2].read())
        check("S7: success result carried the staged-change summary",
              "main.c: +1 -1" in turns[4].read())
        check("S7: the model's answer mentions the staged state",
              "staged in the box" in turns[5].read())
        # The second gen's prompt replayed the SHELL call natively too.
        second_gen = st.srv.requests[2]
        tcs = [m.get("tool_calls") for m in second_gen.messages
               if m.get("tool_calls")]
        check("S7: shell calls replay as native tool_calls",
              tcs and tcs[0][0]["function"]["name"] == "shell")
    finally:
        st.close()


# ── scenario 8: SarunExecutor wiring against a stub sarun binary ─────────────
def test_scenario_sarun_executor_wiring():
    """SarunExecutor drives the real subprocess protocol: `sarun BOX -- sh -c
    SCRIPT`, then `sarun BOX patch`. A stub sarun records argv and plays a
    canned patch, so this verifies the wiring without a UI."""
    tmp = Path(tempfile.mkdtemp(prefix="oaita-stub-"))
    try:
        stub = tmp / "sarun"
        log = tmp / "argv.log"
        stub.write_text(
            "#!/bin/sh\n"
            f"echo \"$@\" >> {log}\n"
            "if [ \"$2\" = patch ]; then\n"
            "  printf -- '--- a/f.txt\\n+++ b/f.txt\\n@@\\n+x\\n'\n"
            "else\n"
            "  echo 'script output here'\n"
            "fi\n")
        stub.chmod(0o755)
        ex = oaita.SarunExecutor(str(stub))
        r = ex.run_script("echo hi", box="OAITA-DEMO")
        calls = log.read_text().splitlines()
        check("S8: run invoked `sarun BOX -- sh -c SCRIPT`",
              calls[0] == "OAITA-DEMO -- sh -c echo hi")
        check("S8: then asked for the box patch",
              calls[1] == "OAITA-DEMO patch")
        check("S8: output captured", "script output here" in r.output)
        check("S8: patch summarized", "f.txt: +1 -0" in r.changes)
        check("S8: exit code propagated", r.exit_code == 0)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


# ── scenario 9: count lines across a directory — the full multi-agent arc ────
def test_scenario_count_lines_across_dir():
    """The big one. Main inspects a directory, delegates each file to a tool-
    capable sub-agent that fumbles `wc`, recovers, then COLLAPSES its messy
    attempt log into one clean answer via `backtrack(final=true)`; that clean
    turn rides back up as the tool result and kicks the parent forward; main
    keeps a running tally and — deliberately — never deletes the spent
    sub-agent sessions (the leak wart, observed)."""
    st = Stage(seed="count")
    try:
        # A real directory of real files (inspect is harness-native).
        work = st.tmp / "src"
        work.mkdir()
        (work / "a.txt").write_text("l1\nl2\nl3\n")          # 3 lines
        (work / "b.txt").write_text("only one line\n")        # 1 line

        st.write_turn("tally", "0010.user",
                      f"Count the lines in every file under {work}.")

        # ── main's reasoning, keyed on what it has just seen ──
        # 1. first sees the task → inspect the directory.
        st.srv.expect(f"Count the lines in every file under {work}.",
                      CannedChat(tool_calls=[
                          ("inspect", json.dumps({"path": str(work)}), "c0")]))
        # 2. sees the listing → delegate a.txt (with a tally preamble: 0 so far).
        st.srv.expect(lambda r: _last(r).startswith(f"{work} (2 entries)"),
                      CannedChat(
                          content="0 lines so far. Counting a.txt.",
                          tool_calls=[("act", json.dumps(
                              {"request": f"count lines in {work}/a.txt"}),
                              "call_filea")]))
        # 3. sees a.txt's result (3) → tally 3, delegate b.txt.
        st.srv.expect(lambda r: "a.txt has 3 lines" in _last(r),
                      CannedChat(
                          content="3 lines so far. Counting b.txt.",
                          tool_calls=[("act", json.dumps(
                              {"request": f"count lines in {work}/b.txt"}),
                              "call_fileb")]))
        # 4. sees b.txt's result (1) → final clean answer (no more calls).
        st.srv.expect(lambda r: "b.txt has 1 line" in _last(r),
                      CannedChat(content="Total: 4 lines (a.txt 3, b.txt 1)."))

        # ── each sub-agent: wrong wc, error, right wc, then collapse ──
        # The sub-agent first runs a broken wc, sees the error, re-runs it
        # correctly, then deletes its own call-chain into a clean annotation.
        for fid, fname, nlines, line_word in (
                ("filea", "a.txt", 3, "lines"),
                ("fileb", "b.txt", 1, "line")):
            st.srv.expect(lambda r, fn=fname: f"count lines in {work}/{fn}"
                          in _last(r) and not _has_tool(r, "shell"),
                          CannedChat(tool_calls=[("shell", json.dumps(
                              {"script": f"wc --count-lines {work}/{fname}"}),
                              f"call_{fid}1")]))
            # sees the REAL wc error → run it correctly.
            st.srv.expect(lambda r, fn=fname: "unrecognized option" in _last(r)
                          and fn in " ".join(
                              m.get("content") or "" for m in r.messages),
                          CannedChat(tool_calls=[("shell", json.dumps(
                              {"script": f"wc -l < {work}/{fname}"}),
                              f"call_{fid}2")]))
            # sees the correct count → collapse the whole mess into one
            # FINAL answer, rewinding to the FIRST (broken) shell call turn.
            st.srv.expect(lambda r, n=nlines: f"exit 0\n{n}" in _last(r),
                          CannedChat(tool_calls=[("backtrack", json.dumps(
                              {"turn_id": f"{fid}1", "final": True,
                               "summary": f"{fname} has {nlines} "
                                          f"{line_word}."}),
                              f"call_{fid}d")]))

        # LocalExecutor end to end: the sub-agents are real `oaita run`
        # subprocesses, and their wc invocations hit the REAL wc binary —
        # the fumbled flag really errors, the fixed one really counts.
        produced = st.run("tally", executor=oaita.LocalExecutor(),
                          max_steps=40)

        # ── main settled with the running tally visible ──
        main = turns_of("tally")
        types = [t.type for t in main]
        check("S9: main ran inspect, two delegations, and tallied",
              types == ["user",
                        "assistant",            # inspect call (c)
                        "tool",                 # listing
                        "assistant",            # "0 lines so far" narration
                        "assistant",            # act a.txt (c)
                        "tool",                 # a.txt result
                        "assistant",            # "3 lines so far" narration
                        "assistant",            # act b.txt (c)
                        "tool",                 # b.txt result
                        "assistant"])           # final total
        narrations = [t.read() for t in main
                      if t.type == "assistant" and "c" not in t.flags]
        check("S9: the running tally is preserved each step (not overwritten)",
              narrations == ["0 lines so far. Counting a.txt.",
                             "3 lines so far. Counting b.txt.",
                             "Total: 4 lines (a.txt 3, b.txt 1)."])
        # The two delegation RESULTS are the sub-agents' collapsed annotations.
        results = [t.read() for t in main if t.type == "tool"]
        check("S9: a.txt sub-agent returned the clean collapsed annotation",
              "a.txt has 3 lines." in results)
        check("S9: b.txt sub-agent returned the clean collapsed annotation",
              "b.txt has 1 line." in results)

        # ── each sub-agent collapsed its messy attempt log ──
        sub_a = turns_of("filea")
        check("S9: sub-agent a collapsed to seed-user + ONE clean annotation",
              [t.type for t in sub_a] == ["user", "assistant"]
              and sub_a[-1].read() == "a.txt has 3 lines."
              and sub_a[-1].flags == "")
        check("S9: the broken-wc attempt turns are GONE after rollback",
              not any("wc --lines" in t.read() for t in sub_a))
        check("S9: sub-agent seed carries from=<main>", sub_a[0].sender == "tally")

        # ── the leak wart: spent sub-agent sessions were never cleaned up ──
        sibling = {d.name for d in oaita.state_home().iterdir() if d.is_dir()}
        check("S9: WART — spent sub-agent sessions leak (main never deletes)",
              {"filea", "fileb"} <= sibling)

        # ── the wc executor really saw both the wrong and right invocations ──
        check("S9: every scripted model step fired — incl. both real-wc "
              "fumble-and-fix paths",
              all(e.matched for e in st.srv._expectations))
    finally:
        st.close()


# ── scenario 10: in-context compaction via backtrack — context SHRINKS ───────
def test_scenario_compaction_via_backtrack():
    """Compaction as an ordinary tool call, no harness magic: mid-task the
    model rewinds its own done-and-banked stretch (two measurement arcs) into
    one waypoint, then KEEPS WORKING — and the next request's context really
    is smaller: the raw outputs are gone, only the waypoint rides forward.
    This is the second mechanism next to boxes: a boxed compactor could
    rewrite session files, but the tool-result turns the CALLER wrote outside
    the box are not the box's to delete — backtrack is."""
    st = Stage(seed="compact")
    try:
        st.write_turn("job", "0010.user",
                      "Measure A, B and C; report the total.")

        # 1-2. two real measurement arcs (LocalExecutor → real sh).
        st.srv.expect("Measure A, B and C",
                      CannedChat(tool_calls=[("shell", json.dumps(
                          {"script": "echo A=5"}), "call_measa")]))
        st.srv.expect(lambda r: "A=5" in _last(r),
                      CannedChat(tool_calls=[("shell", json.dumps(
                          {"script": "echo B=7"}), "call_measb")]))
        # 3. both banked → COMPACT: rewind to the first call turn (its slug
        #    is the adopted wire id), carry only the waypoint summary.
        st.srv.expect(lambda r: "B=7" in _last(r),
                      CannedChat(tool_calls=[("backtrack", json.dumps(
                          {"turn_id": "measa",
                           "summary": "Done: two measurements banked, "
                                      "partial sum 12."}), "call_compact")]))
        # 4. the waypoint is now the tail → proceed with C.
        st.srv.expect("partial sum 12",
                      CannedChat(tool_calls=[("shell", json.dumps(
                          {"script": "echo C=2"}), "call_measc")]))
        # 5. C banked → the finished answer.
        st.srv.expect(lambda r: "C=2" in _last(r),
                      CannedChat(content="Total: 14 (partial 12 + C 2)."))

        st.run("job", executor=oaita.LocalExecutor(), max_steps=12)

        turns = turns_of("job")
        check("S10: settled tree is user, waypoint, C-call, result, answer",
              [t.type for t in turns] ==
              ["user", "assistant", "assistant", "tool", "assistant"]
              and turns[1].flags == "b" and "c" in turns[2].flags)
        check("S10: the waypoint carries the compacted record",
              turns[1].read() == "Done: two measurements banked, "
                                 "partial sum 12.")
        check("S10: the final answer used the waypoint",
              turns[-1].read() == "Total: 14 (partial 12 + C 2).")

        # The compaction POINT: the post-compaction request really shrank —
        # the A/B arcs (calls and raw outputs) are no longer in the context.
        post = st.srv.requests[3]
        sent = " ".join(m.get("content") or "" for m in post.messages)
        check("S10: the post-compaction context no longer carries raw A/B",
              "A=5" not in sent and "B=7" not in sent
              and "partial sum 12" in sent)
        check("S10: the post-compaction prompt is exactly 2 messages "
              "(the user task + the waypoint)",
              len(post.messages) == 2)
        check("S10: every scripted step fired",
              all(e.matched for e in st.srv._expectations))
    finally:
        st.close()


# ── scenario 11: the trace crosses the process boundary ──────────────────────
def test_scenario_trace_across_processes():
    """The flight recorder's reason to exist: a delegation arc where the
    sub-agent is a REAL `oaita run` subprocess, and BOTH processes stream to
    the one collector ($OAITA_TRACE rides the environment — exactly how it
    rides into a sarun box, which inherits env and shares the host netns)."""
    st = Stage(seed="traced")
    col = oaita.TraceCollector("127.0.0.1:0").start()
    os.environ["OAITA_TRACE"] = col.endpoint
    try:
        _play_delegation(st)

        deadline = time.time() + 10
        while time.time() < deadline and not any(
                e["event"] == "run.settled" and e.get("depth") == 0
                for e in col.events):
            time.sleep(0.05)
        pids = {e["pid"] for e in col.events}
        check("S11: at least two PROCESSES reported to the one collector",
              len(pids) >= 2)
        check("S11: the sub-agent's events arrive at depth 1",
              any(e.get("depth") == 1 and e["event"] == "gen.request"
                  for e in col.events))
        check("S11: the outer arc settled at depth 0",
              any(e["event"] == "run.settled" and e.get("depth") == 0
                  and e.get("answer") == "Use MIT for the parser."
                  for e in col.events))
        inner = [e for e in col.events if e.get("depth") == 1]
        check("S11: the inner generation is fully visible from outside "
              "(prompt and reply) without touching its files",
              any(e["event"] == "gen.request"
                  and any("research MIT vs GPL" in (m.get("content") or "")
                          for m in e.get("messages", [])) for e in inner)
              and any(e["event"] == "run.settled" for e in inner))
    finally:
        os.environ.pop("OAITA_TRACE", None)
        col.stop()
        st.close()


# ── standalone runner ────────────────────────────────────────────────────────
if __name__ == "__main__":
    tests = [
        test_scenario_delegation_and_follow_up,
        test_scenario_partial_resume,
        test_scenario_parallel_fanout,
        test_scenario_determinism_replay,
        test_scenario_cli_subprocess,
        test_scenario_shell_fix_build,
        test_scenario_sarun_executor_wiring,
        test_scenario_count_lines_across_dir,
        test_scenario_compaction_via_backtrack,
        test_scenario_trace_across_processes,
    ]
    for t in tests:
        print(f"\n── {t.__name__} ──")
        try:
            t()
        except Exception:
            import traceback
            traceback.print_exc()
            _fails.append(t.__name__)
    print("\n" + ("ALL PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    sys.exit(1 if _fails else 0)
