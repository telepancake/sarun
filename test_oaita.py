#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "openai>=1.30",
#   "pytest>=8",
# ]
# ///
"""Tests for the `oaita` resumable chat client.

Drives `oaita`'s public `generate(...)` in-process against a live
`oaita_fakeserver.FakeOpenAIServer`, pointing base_url at the fake and using a
TEMP dir as `$XDG_STATE_HOME` so every session is isolated on disk.

Covered:
  1. Filename grammar + alphabetical==turn ordering; non-conforming files ignored.
  2. Append: a new `0020.assistant` is created with the raw reply; the prompt
     contains only the real prior turns (no invented messages).
  3. Continue/regenerate: a trailing assistant turn is rewritten in place (no new
     file) and excluded from the prompt.
  4. Numbering with slugs: highest+10 across slugged files.
  5. Roles/order: a system+user+assistant history is sent in order with roles.
  6. Streaming: multi-chunk content reassembles on disk and the server saw
     `stream:true`.
  7. Empty/missing session: raises and writes no files.

Dual style: standalone (`./test_oaita.py` → `ALL PASS`) and pytest-compatible.
"""
import os
import shutil
import sys
import tempfile
from importlib.machinery import SourceFileLoader
from pathlib import Path

HERE = Path(__file__).resolve().parent
oaita = SourceFileLoader("oaita", str(HERE / "oaita")).load_module()
fakeserver = SourceFileLoader(
    "oaita_fakeserver", str(HERE / "oaita_fakeserver")).load_module()

CannedChat = fakeserver.CannedChat

_fails = []


def check(msg, cond):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond:
        _fails.append(msg)


# ── test fixtures ────────────────────────────────────────────────────────────
class Session:
    """A throwaway $XDG_STATE_HOME + a live fake server, for one test."""

    def __init__(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="oaita-test-"))
        self._prev_xdg = os.environ.get("XDG_STATE_HOME")
        os.environ["XDG_STATE_HOME"] = str(self.tmp / "state")
        self.srv = fakeserver.FakeOpenAIServer().start()

    def close(self):
        try:
            self.srv.stop()
        finally:
            if self._prev_xdg is None:
                os.environ.pop("XDG_STATE_HOME", None)
            else:
                os.environ["XDG_STATE_HOME"] = self._prev_xdg
            shutil.rmtree(self.tmp, ignore_errors=True)

    def write_turn(self, name, filename, content):
        folder = oaita.session_dir(name)
        folder.mkdir(parents=True, exist_ok=True)
        (folder / filename).write_text(content, encoding="utf-8")

    def generate(self, name):
        return oaita.generate(
            name,
            model="test-model",
            base_url=self.srv.base_url,
            api_key="test-key",
            echo=lambda _text: None,
        )


# ── 1. filename grammar + ordering ───────────────────────────────────────────
def test_grammar_and_ordering():
    t1 = oaita.parse_turn(Path("0010.user"))
    t2 = oaita.parse_turn(Path("0010-greet.user"))
    check("0010.user parses to (10, None, user)",
          t1 is not None and (t1.number, t1.slug, t1.type) == (10, None, "user"))
    check("0010-greet.user parses to (10, 'greet', user)",
          t2 is not None and
          (t2.number, t2.slug, t2.type) == (10, "greet", "user"))
    check("role == type", t1 is not None and t1.role == "user")
    check("notes.txt is not a turn (ignored)",
          oaita.parse_turn(Path("notes.txt")) is None)
    check("0010.bogus (bad type) is not a turn",
          oaita.parse_turn(Path("0010.bogus")) is None)

    s = Session()
    try:
        name = "ordering"
        # Deliberately create out of insertion order; also a non-turn file.
        s.write_turn(name, "0030.assistant", "third")
        s.write_turn(name, "0010.user", "first")
        s.write_turn(name, "0020-q.user", "second")
        s.write_turn(name, "notes.txt", "ignored")
        turns = oaita.load_turns(name)
        check("non-turn file ignored in load_turns", len(turns) == 3)
        check("alphabetical sort == turn order",
              [t.number for t in turns] == [10, 20, 30])
        check("contents read raw in order",
              [t.read() for t in turns] == ["first", "second", "third"])
        check("next_number is highest+10", oaita.next_number(turns) == 40)
    finally:
        s.close()


# ── 2. append ────────────────────────────────────────────────────────────────
def test_append_creates_assistant_turn():
    s = Session()
    try:
        name = "append"
        s.write_turn(name, "0010.user", "hello")
        s.srv.enqueue(CannedChat(content="hi there"))
        produced = s.generate(name)
        folder = oaita.session_dir(name)
        target = folder / "0020.assistant"
        check("a single turn produced", len(produced) == 1)
        check("produced path is 0020.assistant", produced[0] == target)
        check("0020.assistant file exists", target.is_file())
        check("content is exactly the raw reply (no JSON/wrapping)",
              target.read_text() == "hi there")
        names = sorted(p.name for p in folder.iterdir())
        check("exactly 0010.user + 0020.assistant on disk",
              names == ["0010.user", "0020.assistant"])
        req = s.srv.requests[-1]
        check("prompt did not invent extra turns",
              req.messages == [{"role": "user", "content": "hello"}])
    finally:
        s.close()


# ── 3. continue / regenerate in place ────────────────────────────────────────
def test_regenerate_in_place():
    s = Session()
    try:
        name = "regen"
        s.write_turn(name, "0010.user", "hello")
        s.write_turn(name, "0020.assistant", "stale partial")
        s.srv.enqueue(CannedChat(content="regenerated"))
        produced = s.generate(name)
        folder = oaita.session_dir(name)
        target = folder / "0020.assistant"
        check("no new file created (regenerate in place)",
              produced == [target])
        names = sorted(p.name for p in folder.iterdir())
        check("still exactly 0010.user + 0020.assistant",
              names == ["0010.user", "0020.assistant"])
        check("0020.assistant rewritten to new content",
              target.read_text() == "regenerated")
        req = s.srv.requests[-1]
        check("prompt EXCLUDED the trailing assistant turn",
              req.messages == [{"role": "user", "content": "hello"}])
    finally:
        s.close()


# ── 4. numbering with slugs ──────────────────────────────────────────────────
def test_numbering_with_slugs():
    s = Session()
    try:
        name = "slugs"
        s.write_turn(name, "0010-greet.user", "hi")
        s.write_turn(name, "0020-q.user", "what is 2+2?")
        s.srv.enqueue(CannedChat(content="4"))
        produced = s.generate(name)
        target = oaita.session_dir(name) / "0030.assistant"
        check("appends 0030.assistant (highest+10 across slugged files)",
              produced == [target] and target.is_file())
        check("appended assistant has no slug",
              target.name == "0030.assistant")
    finally:
        s.close()


# ── 5. roles / order ─────────────────────────────────────────────────────────
def test_roles_and_order():
    s = Session()
    try:
        name = "roles"
        s.write_turn(name, "0010.system", "be terse")
        s.write_turn(name, "0020.user", "ping")
        s.write_turn(name, "0030.assistant", "pong")
        # Last turn is assistant → regenerate; prompt = system + user.
        s.srv.enqueue(CannedChat(content="pong2"))
        s.generate(name)
        req = s.srv.requests[-1]
        check("history sent in order with correct roles",
              req.messages == [
                  {"role": "system", "content": "be terse"},
                  {"role": "user", "content": "ping"},
              ])
    finally:
        s.close()


# ── 6. streaming raw content ─────────────────────────────────────────────────
def test_streaming_reassembles():
    s = Session()
    try:
        name = "stream"
        s.write_turn(name, "0010.user", "tell me a story")
        full = "once upon a time there was a tiny client"
        s.srv.enqueue(CannedChat(content=full, n_content_chunks=7))
        produced = s.generate(name)
        check("streamed content reassembles exactly on disk",
              produced[0].read_text() == full)
        check("server saw stream:true",
              s.srv.requests[-1].json["stream"] is True)
    finally:
        s.close()


# ── 7. empty / missing session ───────────────────────────────────────────────
def test_empty_and_missing_session():
    s = Session()
    try:
        # Missing folder entirely.
        raised = False
        try:
            s.generate("does-not-exist")
        except SystemExit:
            raised = True
        check("missing session raises non-zero (SystemExit)", raised)
        check("no folder created for missing session",
              not oaita.session_dir("does-not-exist").exists())

        # Folder exists but holds only a non-turn file → still empty of turns.
        name = "only-junk"
        s.write_turn(name, "notes.txt", "not a turn")
        raised2 = False
        try:
            s.generate(name)
        except SystemExit:
            raised2 = True
        check("session with no turns raises non-zero", raised2)
        files = sorted(p.name for p in oaita.session_dir(name).iterdir())
        check("no empty turn files created", files == ["notes.txt"])
    finally:
        s.close()


# ── standalone runner ────────────────────────────────────────────────────────
if __name__ == "__main__":
    tests = [
        test_grammar_and_ordering,
        test_append_creates_assistant_turn,
        test_regenerate_in_place,
        test_numbering_with_slugs,
        test_roles_and_order,
        test_streaming_reassembles,
        test_empty_and_missing_session,
    ]
    for t in tests:
        try:
            t()
        except Exception:
            import traceback
            traceback.print_exc()
            _fails.append(t.__name__)
    print("\n" + ("ALL PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    sys.exit(1 if _fails else 0)
