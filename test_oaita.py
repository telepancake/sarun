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
  2. Append: a new `0020-<id>.assistant` is created with the raw reply; the prompt
     contains only the real prior turns (no invented messages), each with the
     injected turn-id header.
  3. Continue/regenerate: a trailing assistant turn is rewritten in place (no new
     file) and excluded from the prompt; its slug/id is stable across regenerations.
  4. Numbering with slugs: highest+10 across slugged files.
  5. Roles/order: a system+user+assistant history is sent in order with roles.
  6. Streaming: multi-chunk content reassembles on disk and the server saw
     `stream:true`.
  7. Empty/missing session: raises and writes no files.
  8. Slug assignment + rename: slug-less turns get a generated slug on disk.
  9. Existing slug preserved as-is (user label becomes turn-id verbatim).
 10. Injected header on the wire: every message carries {"turn-id": "<id>"}\n.
 11. Files stay raw: no turn file on disk contains "turn-id" after generation.
 12. Generated assistant turn gets a slug matching ^[a-z]{5}$.
 13. Uniqueness: pairwise-distinct ids across a session with many slug-less turns.
 14. Regenerate keeps id stable: second generate does not change the tail's slug.

Dual style: standalone (`./test_oaita.py` → `ALL PASS`) and pytest-compatible.
"""
import json
import os
import re
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


# ── helpers ──────────────────────────────────────────────────────────────────
_ID_RE = re.compile(r"^[a-z]{5}$")


def _id_from_name(name: str) -> str | None:
    """Extract the slug from a turn filename, e.g. '0020-xqvmb.assistant' → 'xqvmb'."""
    mo = re.match(r"^\d+-([^.]+)\.", name)
    return mo.group(1) if mo else None


def _header_for(slug: str) -> str:
    """The expected injected header line for a given slug."""
    return json.dumps({"turn-id": slug}) + "\n"


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

        # The produced path should be 0020-<5-letter-id>.assistant.
        check("a single turn produced", len(produced) == 1)
        target = produced[0]
        check("produced file is in session folder", target.parent == folder)
        tname = target.name
        check("produced filename matches 0020-<id>.assistant pattern",
              bool(re.match(r"^0020-[a-z]{5}\.assistant$", tname)))
        check("produced file exists", target.is_file())
        check("content is exactly the raw reply (no JSON/wrapping)",
              target.read_text() == "hi there")

        # Disk should hold: 0010-<id>.user (renamed) + 0020-<id>.assistant
        files = sorted(p.name for p in folder.iterdir())
        check("exactly two turn files on disk", len(files) == 2)
        check("first file is 0010-<id>.user",
              bool(re.match(r"^0010-[a-z]{5}\.user$", files[0])))
        check("second file is 0020-<id>.assistant",
              bool(re.match(r"^0020-[a-z]{5}\.assistant$", files[1])))
        check("original 0010.user no longer exists",
              not (folder / "0010.user").exists())

        # The prompt sent to the server must carry the id-header.
        req = s.srv.requests[-1]
        check("prompt has exactly one message", len(req.messages) == 1)
        user_slug = _id_from_name(files[0])
        expected_content = _header_for(user_slug) + "hello"
        check("user message carries injected turn-id header",
              req.messages[0]["content"] == expected_content)
        check("user message role is correct",
              req.messages[0]["role"] == "user")
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

        # Both turns get slugs; the old slug-less 0020.assistant is renamed.
        files = sorted(p.name for p in folder.iterdir())
        check("exactly two turn files on disk after regen", len(files) == 2)
        check("user turn has slug", bool(re.match(r"^0010-[a-z]{5}\.user$", files[0])))
        check("assistant turn has slug",
              bool(re.match(r"^0020-[a-z]{5}\.assistant$", files[1])))
        check("original slug-less 0020.assistant gone",
              not (folder / "0020.assistant").exists())

        # produced[0] should point at the (renamed) assistant file.
        target = produced[0]
        check("no new file created (regenerate in place)", produced == [target])
        check("produced path matches the renamed assistant file",
              target.name == files[1])
        check("0020-<id>.assistant rewritten to new content",
              target.read_text() == "regenerated")

        # Prompt was sent with only the user turn (assistant excluded).
        req = s.srv.requests[-1]
        check("prompt EXCLUDED the trailing assistant turn",
              len(req.messages) == 1 and req.messages[0]["role"] == "user")
        user_slug = _id_from_name(files[0])
        check("user message carries injected turn-id header",
              req.messages[0]["content"] == _header_for(user_slug) + "hello")
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
        folder = oaita.session_dir(name)
        tname = produced[0].name
        check("appended turn is 0030-<id>.assistant (highest+10)",
              bool(re.match(r"^0030-[a-z]{5}\.assistant$", tname)))
        check("file exists on disk", produced[0].is_file())
        # User-authored slugs must survive intact.
        check("0010-greet.user kept its slug", (folder / "0010-greet.user").exists())
        check("0020-q.user kept its slug", (folder / "0020-q.user").exists())
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
        folder = oaita.session_dir(name)
        req = s.srv.requests[-1]
        # After slug assignment the files are e.g. 0010-<id>.system, 0020-<id>.user.
        files = sorted(p.name for p in folder.iterdir())
        sys_name = next(f for f in files if f.endswith(".system"))
        usr_name = next(f for f in files if f.endswith(".user"))
        sys_slug = _id_from_name(sys_name)
        usr_slug = _id_from_name(usr_name)
        check("history sent in order with correct roles",
              req.messages == [
                  {"role": "system",
                   "content": _header_for(sys_slug) + "be terse"},
                  {"role": "user",
                   "content": _header_for(usr_slug) + "ping"},
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
        # File on disk must be the raw reply — no header.
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


# ── 8. slug assignment + rename ──────────────────────────────────────────────
def test_slug_assignment_and_rename():
    """A slug-less 0010.user is renamed to 0010-<id>.user on disk."""
    s = Session()
    try:
        name = "rename"
        s.write_turn(name, "0010.user", "hello")
        s.srv.enqueue(CannedChat(content="hi"))
        s.generate(name)
        folder = oaita.session_dir(name)
        files = sorted(p.name for p in folder.iterdir())
        user_files = [f for f in files if f.endswith(".user")]
        check("exactly one user file on disk", len(user_files) == 1)
        check("original 0010.user no longer exists",
              not (folder / "0010.user").exists())
        uf = user_files[0]
        slug = _id_from_name(uf)
        check("renamed user file matches 0010-<id>.user",
              bool(re.match(r"^0010-[a-z]{5}\.user$", uf)))
        check("extracted slug matches [a-z]{5}",
              slug is not None and bool(_ID_RE.match(slug)))
        # File content must still be the raw original text.
        check("renamed file still holds raw content",
              (folder / uf).read_text() == "hello")
    finally:
        s.close()


# ── 9. existing slug preserved as turn-id ────────────────────────────────────
def test_existing_slug_preserved():
    """A pre-slugged file keeps its slug; the injected header uses it verbatim."""
    s = Session()
    try:
        name = "keepslug"
        s.write_turn(name, "0010-greet.user", "hello")
        s.srv.enqueue(CannedChat(content="hi"))
        s.generate(name)
        folder = oaita.session_dir(name)
        # The file must not have been renamed.
        check("0010-greet.user still exists (not renamed)",
              (folder / "0010-greet.user").exists())
        req = s.srv.requests[-1]
        check("prompt has one message", len(req.messages) == 1)
        # The header must be exactly {"turn-id": "greet"}\n.
        expected = '{"turn-id": "greet"}\nhello'
        check("injected header uses slug verbatim as turn-id",
              req.messages[0]["content"] == expected)
    finally:
        s.close()


# ── 10. injected header on the wire for every message ────────────────────────
def test_header_injected_on_wire():
    """Every sent message has {"turn-id": "<id>"}\n prepended; files stay raw."""
    s = Session()
    try:
        name = "wire"
        s.write_turn(name, "0010.system", "be helpful")
        s.write_turn(name, "0020.user", "hello world")
        s.srv.enqueue(CannedChat(content="reply"))
        s.generate(name)
        folder = oaita.session_dir(name)
        req = s.srv.requests[-1]
        check("two messages sent to server", len(req.messages) == 2)
        for msg in req.messages:
            content = msg["content"]
            check(f"message role={msg['role']!r} has turn-id header line",
                  content.startswith('{"turn-id": "') and "\n" in content)
            # The header must be valid JSON with exactly the key "turn-id".
            header_line, raw = content.split("\n", 1)
            parsed = json.loads(header_line)
            check(f"role={msg['role']!r} header parses to dict with turn-id key",
                  isinstance(parsed, dict) and "turn-id" in parsed)
            check(f"role={msg['role']!r} turn-id value matches [a-z]{{5}}",
                  bool(_ID_RE.match(parsed["turn-id"])))
        # Files on disk must NOT contain "turn-id".
        for p in folder.iterdir():
            if p.is_file() and oaita.parse_turn(p) is not None:
                check(f"file {p.name} does not contain 'turn-id' on disk",
                      "turn-id" not in p.read_text())
    finally:
        s.close()


# ── 11. files stay raw ───────────────────────────────────────────────────────
def test_files_stay_raw():
    """After generation, NO turn file on disk contains the string 'turn-id'."""
    s = Session()
    try:
        name = "raw"
        s.write_turn(name, "0010.user", "ping")
        reply = "pong"
        s.srv.enqueue(CannedChat(content=reply))
        produced = s.generate(name)
        folder = oaita.session_dir(name)
        # The generated assistant file must be exactly the raw model reply.
        check("generated assistant file contains exactly the raw reply",
              produced[0].read_text() == reply)
        # No turn file anywhere should hold the header text.
        for p in folder.iterdir():
            if p.is_file():
                check(f"file {p.name} contains no 'turn-id' substring",
                      "turn-id" not in p.read_text())
    finally:
        s.close()


# ── 12. generated assistant turn gets a slug ─────────────────────────────────
def test_generated_assistant_has_slug():
    """The appended assistant turn file matches ^0020-[a-z]{5}\\.assistant$."""
    s = Session()
    try:
        name = "aslug"
        s.write_turn(name, "0010-hi.user", "hello")
        s.srv.enqueue(CannedChat(content="world"))
        produced = s.generate(name)
        check("produced assistant matches 0020-<id>.assistant",
              bool(re.match(r"^0020-[a-z]{5}\.assistant$", produced[0].name)))
    finally:
        s.close()


# ── 13. uniqueness across many slug-less turns ───────────────────────────────
def test_uniqueness_of_generated_ids():
    """Pairwise-distinct ids when a session has several slug-less turns."""
    s = Session()
    try:
        name = "unique"
        # Write 5 slug-less turns (all user so there is no trailing assistant).
        for i in range(1, 6):
            s.write_turn(name, f"{i*10:04d}.user", f"turn {i}")
        s.srv.enqueue(CannedChat(content="done"))
        s.generate(name)
        folder = oaita.session_dir(name)
        files = sorted(p.name for p in folder.iterdir()
                       if oaita.parse_turn(p) is not None)
        slugs = [_id_from_name(f) for f in files]
        check("all 6 turns have slugs (5 user + 1 assistant)",
              len(slugs) == 6 and all(s is not None for s in slugs))
        check("all slugs are distinct", len(set(slugs)) == len(slugs))
        check("all slugs match [a-z]{5}",
              all(_ID_RE.match(sl) for sl in slugs if sl))
    finally:
        s.close()


# ── 14. regenerate keeps id stable ───────────────────────────────────────────
def test_regenerate_keeps_id_stable():
    """A second generate on an already-slugged assistant tail keeps the same id."""
    s = Session()
    try:
        name = "stable"
        s.write_turn(name, "0010-hi.user", "hello")
        # Pre-write a slugged assistant tail.
        s.write_turn(name, "0020-oldid.assistant", "stale")
        s.srv.enqueue(CannedChat(content="fresh"))
        produced = s.generate(name)
        folder = oaita.session_dir(name)
        # The tail's filename must not have changed.
        check("regenerated file keeps original slug 'oldid'",
              produced[0].name == "0020-oldid.assistant")
        check("file still exists with same name",
              (folder / "0020-oldid.assistant").exists())
        check("content overwritten to new reply",
              produced[0].read_text() == "fresh")
        # And a second regeneration still keeps it.
        s.srv.enqueue(CannedChat(content="fresher"))
        produced2 = s.generate(name)
        check("second regeneration still uses oldid slug",
              produced2[0].name == "0020-oldid.assistant")
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
        test_slug_assignment_and_rename,
        test_existing_slug_preserved,
        test_header_injected_on_wire,
        test_files_stay_raw,
        test_generated_assistant_has_slug,
        test_uniqueness_of_generated_ids,
        test_regenerate_keeps_id_stable,
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
