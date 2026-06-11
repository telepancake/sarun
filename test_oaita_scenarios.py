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
    """A throwaway $XDG_STATE_HOME + live fake server + pinned id seed."""

    def __init__(self, seed="scenario"):
        self.tmp = Path(tempfile.mkdtemp(prefix="oaita-scen-"))
        self._prev_xdg = os.environ.get("XDG_STATE_HOME")
        self._prev_seed = os.environ.get("OAITA_ID_SEED")
        os.environ["XDG_STATE_HOME"] = str(self.tmp / "state")
        os.environ["OAITA_ID_SEED"] = seed
        self.srv = fakeserver.FakeOpenAIServer().start()

    def close(self):
        try:
            self.srv.stop()
        finally:
            for key, prev in (("XDG_STATE_HOME", self._prev_xdg),
                              ("OAITA_ID_SEED", self._prev_seed)):
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
    return st.run("main")


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
        check("S1: outer gens advertise act; the leaf advertises nothing",
              reqs[0].tools and reqs[0].tools[0]["function"]["name"] == "act"
              and reqs[2].tools and reqs[1].tools is None)
        # The synthesis prompt narrates the call: assistant turn = envelope
        # JSON as content, tool turn with the from-bearing header.
        synth = reqs[2].messages
        check("S1: synthesis prompt is user, assistant(envelope), tool",
              [m["role"] for m in synth] == ["user", "assistant", "tool"])
        check("S1: tool result message carries from=<inner> in its header",
              synth[2]["content"].startswith(
                  '{"turn-id": "%s", "from": "%s"}\n'
                  % (result_t.slug, innerid)))

        # ── round 2: follow up the SAME researcher by its from-address ──
        st.write_turn("main", "0050.user",
                      "Ask the researcher specifically about GPLv2 vs v3.")
        st.srv.expect("GPLv2 vs v3",
                      act_call("GPLv2 vs GPLv3?", follow_up=innerid))
        st.srv.expect("GPLv2 vs GPLv3?",
                      CannedChat(content="v3 adds patent clauses."))
        st.srv.expect("v3 adds patent clauses",
                      CannedChat(content="GPLv3 then."))
        st.run("main")

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
                     in r.messages[-1]["content"] and r.tools is None)
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
        st.run("fan")

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
        st.write_turn("clidemo", "0010.user",
                      "Compare MIT and GPL for our parser; "
                      "delegate the license research.")
        _script_delegation(st.srv)
        env = dict(os.environ,
                   OPENAI_BASE_URL=st.srv.base_url,
                   OPENAI_API_KEY="test-key",
                   OAITA_MODEL="test-model")
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


# ── standalone runner ────────────────────────────────────────────────────────
if __name__ == "__main__":
    tests = [
        test_scenario_delegation_and_follow_up,
        test_scenario_partial_resume,
        test_scenario_parallel_fanout,
        test_scenario_determinism_replay,
        test_scenario_cli_subprocess,
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
