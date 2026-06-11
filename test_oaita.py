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
 15. Sloppy parsing of a model-emitted turn-id header (pure function): quote/
     key/whitespace variants stripped; non-leading headers left alone.
 16. Append adopts a valid, unique model-emitted id (and strips the header).
 17. Append rejects a duplicate model id, keeping the generated one (still strips).
 18. Append rejects an invalid (non-slug) model id, keeping the generated one.
 19. Regenerate strips an emitted header but holds the tail's id stable.
 20. Name stitching: 'a.b.c' prepends a,b and infers/writes in the last (c).
 21. Stitch into an empty target session (reply becomes its first turn).
 22. Reordering segments reorders the prepended context.
 23. Regenerate-in-place touches only the target segment's tail.
 24. Name validation: non-alnum, empty segments, and duplicate segments raise.
 25. Turn-ids stay unique across stitched sessions.
 26. Collision guard fires on a pre-existing duplicate turn-id across sessions.
 27. Tool calling happy path: act call → inner sub-agent gen → .tool result; gen
     STOPS (turns user/assistant/tool), inner session named after the call slug
     holds the request + result; returned paths match.
 28. Capabilities surfaced: the advertised single tool is `act` and its
     description embeds the (custom or default) capabilities string.
 29. tool_context stitched: the inner gen's prompt prepends the tool-description
     system turn before the inner user turn.
 30. Follow-up: follow_up continues the existing sub-agent (appends user+assistant
     to it, not duplicated) and yields a new outer .tool result turn.
 31. Always-on: the `act` tool is offered by default; a plain reply is one turn.
 32. gen stops at the tool result (no auto-continuation); a second gen reacts.
 33. Several act calls in one model output are all evaluated, then gen stops.

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

    def generate(self, name, **kw):
        return oaita.generate(
            name,
            model="test-model",
            base_url=self.srv.base_url,
            api_key="test-key",
            echo=lambda _text: None,
            **kw,
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
            s.generate("doesnotexist")
        except SystemExit:
            raised = True
        check("missing session raises non-zero (SystemExit)", raised)
        check("no folder created for missing session",
              not oaita.session_dir("doesnotexist").exists())

        # Folder exists but holds only a non-turn file → still empty of turns.
        name = "onlyjunk"
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


# ── 15. sloppy parsing of a model-emitted turn-id header (pure function) ─────
def test_strip_emitted_header_unit():
    f = oaita.strip_emitted_turn_id
    check("canonical header stripped, id captured",
          f('{"turn-id": "abcde"}\nbody') == ("abcde", "body"))
    check("no-space-after-colon variant stripped",
          f('{"turn-id":"abcde"}\nbody') == ("abcde", "body"))
    check("single-quoted variant stripped",
          f("{'turn-id': 'abcde'}\nbody") == ("abcde", "body"))
    check("underscore key (turn_id) variant stripped",
          f('{"turn_id": "abcde"}\nbody') == ("abcde", "body"))
    check("extra inner whitespace tolerated",
          f('{ "turn-id" : "abcde" }\nbody') == ("abcde", "body"))
    check("leading blank line before header tolerated",
          f('\n{"turn-id": "abcde"}\nbody') == ("abcde", "body"))
    check("header with no trailing newline / no body",
          f('{"turn-id": "abcde"}') == ("abcde", ""))
    check("only the first line is stripped; multi-line body preserved",
          f('{"turn-id": "abcde"}\nl1\nl2') == ("abcde", "l1\nl2"))
    check("plain content is unchanged",
          f("just a normal reply") == (None, "just a normal reply"))
    check("a header NOT at the start is not stripped",
          f('hi\n{"turn-id": "abcde"}\n') == (None, 'hi\n{"turn-id": "abcde"}\n'))


# ── 16. append: a valid, unique model id is adopted; header stripped ─────────
def test_append_adopts_model_id():
    s = Session()
    try:
        name = "adopt"
        s.write_turn(name, "0010-hi.user", "hello")
        # The model imitates the header and picks a valid, unique id.
        s.srv.enqueue(CannedChat(content='{"turn-id": "kitty"}\nthe real body'))
        produced = s.generate(name)
        folder = oaita.session_dir(name)
        check("header stripped from the stored file (raw body only)",
              produced[0].read_text() == "the real body")
        check("model's chosen id adopted as the slug",
              produced[0].name == "0020-kitty.assistant")
        check("no 'turn-id' persisted to disk",
              all("turn-id" not in p.read_text()
                  for p in folder.iterdir() if p.is_file()))
    finally:
        s.close()


# ── 17. append: a duplicate model id is rejected; generated id kept ──────────
def test_append_rejects_duplicate_id():
    s = Session()
    try:
        name = "dup"
        s.write_turn(name, "0010-dupid.user", "hello")
        s.srv.enqueue(CannedChat(content='{"turn-id": "dupid"}\nbody'))
        produced = s.generate(name)
        check("header stripped even when the id is rejected",
              produced[0].read_text() == "body")
        check("colliding id NOT adopted; a generated 5-letter id is used",
              bool(re.match(r"^0020-[a-z]{5}\.assistant$", produced[0].name))
              and produced[0].name != "0020-dupid.assistant")
    finally:
        s.close()


# ── 18. append: an invalid (non-slug) model id is rejected; generated kept ───
def test_append_rejects_invalid_id():
    s = Session()
    try:
        name = "invalid"
        s.write_turn(name, "0010-hi.user", "hello")
        # 'has space' and 'a.b' are not adoptable slugs (space / dot).
        s.srv.enqueue(CannedChat(content='{"turn-id": "has space"}\nbody'))
        produced = s.generate(name)
        check("header stripped even when the id is invalid",
              produced[0].read_text() == "body")
        check("invalid id NOT adopted; a generated 5-letter id is used",
              bool(re.match(r"^0020-[a-z]{5}\.assistant$", produced[0].name)))
    finally:
        s.close()


# ── 19. regenerate: header stripped, but the stable id is NOT changed ────────
def test_regenerate_strips_but_keeps_stable_id():
    s = Session()
    try:
        name = "regenstrip"
        s.write_turn(name, "0010-hi.user", "hello")
        s.write_turn(name, "0020-oldid.assistant", "stale")
        s.srv.enqueue(CannedChat(content='{"turn-id": "newid"}\nregen body'))
        produced = s.generate(name)
        check("regenerated turn keeps its stable id (model id NOT adopted)",
              produced[0].name == "0020-oldid.assistant")
        check("header stripped from the regenerated turn",
              produced[0].read_text() == "regen body")
    finally:
        s.close()


# ── 20. name stitching: prepend earlier segments, infer in the last ─────────
def _bodies(messages):
    """Strip the injected turn-id header line from each message, leaving body."""
    return [m["content"].split("\n", 1)[1] for m in messages]


def test_stitch_prepends_and_targets_last():
    s = Session()
    try:
        s.write_turn("sys", "0010.system", "be terse")
        s.write_turn("conv", "0010.user", "hi")
        s.srv.enqueue(CannedChat(content="hello!"))
        produced = s.generate("sys.conv")  # infer in conv
        conv, sysd = oaita.session_dir("conv"), oaita.session_dir("sys")
        check("reply written into the target (last) segment folder",
              produced[0].parent == conv)
        check("reply is 0020-<id>.assistant within conv",
              bool(re.match(r"^0020-[a-z]{5}\.assistant$", produced[0].name)))
        check("reply stored raw", produced[0].read_text() == "hello!")
        sys_files = sorted(p.name for p in sysd.iterdir())
        check("prepended 'sys' session is NOT written to",
              not any(f.endswith(".assistant") for f in sys_files))
        req = s.srv.requests[-1]
        sys_slug = _id_from_name(next(p.name for p in sysd.iterdir()))
        usr_slug = _id_from_name(
            next(p.name for p in conv.iterdir() if p.name.endswith(".user")))
        check("stitched prompt is [sys.system, conv.user] in order",
              req.messages == [
                  {"role": "system",
                   "content": _header_for(sys_slug) + "be terse"},
                  {"role": "user", "content": _header_for(usr_slug) + "hi"},
              ])
    finally:
        s.close()


# ── 21. stitch into an empty target session ──────────────────────────────────
def test_stitch_into_empty_target():
    s = Session()
    try:
        s.write_turn("skill", "0010.user", "use this skill")
        s.srv.enqueue(CannedChat(content="ok"))
        produced = s.generate("skill.fresh")  # 'fresh' has no turns yet
        fresh = oaita.session_dir("fresh")
        check("reply created as the first turn of the empty target",
              produced[0].parent == fresh and
              bool(re.match(r"^0010-[a-z]{5}\.assistant$", produced[0].name)))
        req = s.srv.requests[-1]
        check("prompt is just the prepended skill turn",
              len(req.messages) == 1 and _bodies(req.messages) == ["use this skill"])
    finally:
        s.close()


# ── 22. reordering segments changes context order ────────────────────────────
def test_stitch_reorder_changes_context_order():
    s = Session()
    try:
        s.write_turn("xxx", "0010.user", "from X")
        s.write_turn("yyy", "0010.user", "from Y")
        s.write_turn("ttt", "0010.user", "go")
        s.srv.enqueue(CannedChat(content="r1"))
        s.generate("xxx.yyy.ttt")
        check("x.y.t context order is [X, Y, go]",
              _bodies(s.srv.requests[-1].messages) == ["from X", "from Y", "go"])
        # ttt now ends with an assistant turn → regenerated in place; reorder x/y.
        s.srv.enqueue(CannedChat(content="r2"))
        s.generate("yyy.xxx.ttt")
        check("y.x.t context order is [Y, X, go] (target tail excluded)",
              _bodies(s.srv.requests[-1].messages) == ["from Y", "from X", "go"])
    finally:
        s.close()


# ── 23. regenerate touches ONLY the target segment's tail ────────────────────
def test_stitch_regenerate_only_target_tail():
    s = Session()
    try:
        s.write_turn("sys", "0010.system", "sys prompt")
        s.write_turn("conv", "0010.user", "q")
        s.write_turn("conv", "0020.assistant", "old answer")
        s.srv.enqueue(CannedChat(content="new answer"))
        produced = s.generate("sys.conv")
        conv = oaita.session_dir("conv")
        files = sorted(p.name for p in conv.iterdir())
        check("conv still has exactly 2 turns (regenerated in place)",
              len(files) == 2)
        check("target tail rewritten to new content",
              produced[0].read_text() == "new answer")
        req = s.srv.requests[-1]
        check("prompt is [system, user]; target assistant tail excluded",
              [m["role"] for m in req.messages] == ["system", "user"])
        check("prompt bodies are the prepended sys prompt then the user q",
              _bodies(req.messages) == ["sys prompt", "q"])
    finally:
        s.close()


# ── 24. name validation: bad chars, empty segments, duplicates ───────────────
def test_name_validation():
    s = Session()
    try:
        for bad in ["bad-name", "a..b", "a.", ".b", "a b", "a.a", ""]:
            raised = False
            try:
                s.generate(bad)
            except SystemExit:
                raised = True
            check(f"invalid/duplicate spec {bad!r} raises SystemExit", raised)
    finally:
        s.close()


# ── 25. turn-ids stay unique across stitched sessions ────────────────────────
def test_stitch_cross_segment_unique_ids():
    s = Session()
    try:
        for i in range(1, 4):
            s.write_turn("alpha", f"{i*10:04d}.user", f"a{i}")
        for i in range(1, 4):
            s.write_turn("beta", f"{i*10:04d}.user", f"b{i}")
        s.srv.enqueue(CannedChat(content="done"))
        s.generate("alpha.beta")  # assistant appended in beta
        slugs = []
        for seg in ("alpha", "beta"):
            for p in oaita.session_dir(seg).iterdir():
                if oaita.parse_turn(p) is not None:
                    slugs.append(_id_from_name(p.name))
        check("all 7 turn-ids present (3 + 3 + 1 generated)", len(slugs) == 7)
        check("turn-ids are distinct across both stitched sessions",
              len(set(slugs)) == len(slugs))
    finally:
        s.close()


# ── 26. collision guard fires on a pre-existing cross-session duplicate ──────
def test_stitch_turn_id_collision_guard():
    s = Session()
    try:
        s.write_turn("one", "0010-dup.user", "x")
        s.write_turn("two", "0010-dup.user", "y")
        raised = False
        try:
            s.generate("one.two")
        except SystemExit:
            raised = True
        check("duplicate turn-id across stitched sessions raises", raised)
    finally:
        s.close()


# ── tool calling: the `act` recursive-sub-agent path ─────────────────────────
def _types_in_order(folder):
    """The turn types of a session folder in turn order (e.g. ['user', ...])."""
    return [oaita.parse_turn(p).type
            for p in sorted(folder.iterdir(), key=lambda q: q.name)
            if oaita.parse_turn(p) is not None]


def _contents_in_order(folder):
    """Raw contents of a session folder in turn order."""
    return [p.read_text()
            for p in sorted(folder.iterdir(), key=lambda q: q.name)
            if oaita.parse_turn(p) is not None]


# ── 27. tool calling happy path ──────────────────────────────────────────────
def test_tools_happy_path():
    s = Session()
    try:
        s.write_turn("conv", "0010.user", "please do X")
        # The model calls act; the inner sub-agent produces the result; `gen`
        # then STOPS (it does not auto-continue the main model).
        s.srv.enqueue(CannedChat(
            tool_calls=[("act", '{"request": "search the thing"}', "call_1")]))
        s.srv.enqueue(CannedChat(content="THE RESULT"))
        produced = s.generate("conv")
        conv = oaita.session_dir("conv")

        check("gen stops at the tool result: turns are user, assistant, tool",
              _types_in_order(conv) == ["user", "assistant", "tool"])
        contents = _contents_in_order(conv)
        check("outer tool-call turn content is the raw request",
              contents[1] == "search the thing")
        check("outer .tool turn content is exactly the inner result",
              contents[2] == "THE RESULT")

        # The tool-call turn's slug names the inner sub-agent session.
        callfile = sorted(p.name for p in conv.iterdir()
                          if p.name.endswith(".assistant"))[0]
        handle = _id_from_name(callfile)
        inner = oaita.session_dir(handle)
        check("inner sub-agent session exists named after the call slug",
              inner.is_dir())
        check("inner session has a .user(request) then .assistant(result)",
              _types_in_order(inner) == ["user", "assistant"])
        icontents = _contents_in_order(inner)
        check("inner user turn is the request", icontents[0] == "search the thing")
        check("inner assistant turn is the result", icontents[1] == "THE RESULT")

        # Returned list = the call + result turns this step wrote, in order.
        check("returned list is the call + result turns in order",
              [p.name for p in produced] ==
              sorted(p.name for p in conv.iterdir()
                     if oaita.parse_turn(p) is not None)[1:])
    finally:
        s.close()


# ── 28. capabilities surfaced in the act tool schema ─────────────────────────
def test_tools_capabilities_surfaced():
    s = Session()
    try:
        s.write_turn("conv", "0010.user", "do X")
        s.srv.enqueue(CannedChat(content="answer"))  # no tool call → one shot
        s.generate("conv", capabilities="bespoke power")
        tools = s.srv.requests[-1].json.get("tools")
        check("a single tool advertised", isinstance(tools, list) and len(tools) == 1)
        fn = tools[0]["function"]
        check("the tool function name is 'act'", fn["name"] == "act")
        check("custom capabilities embedded in the description",
              "bespoke power" in fn["description"])

        # And the default capabilities surface when none is passed.
        s.srv.enqueue(CannedChat(content="answer2"))
        s.write_turn("conv2", "0010.user", "do Y")
        s.generate("conv2")
        fn2 = s.srv.requests[-1].json["tools"][0]["function"]
        check("default capabilities embedded when none passed",
              oaita.DEFAULT_CAPABILITIES in fn2["description"])
    finally:
        s.close()


# ── 29. tool_context stitched before the inner call ──────────────────────────
def test_tools_tool_context_stitched():
    s = Session()
    try:
        s.write_turn("conv", "0010.user", "please do X")
        s.write_turn("tooldesc", "0010.system", "ALL TOOLS HERE")
        s.srv.enqueue(CannedChat(
            tool_calls=[("act", '{"request": "use a tool"}', "call_1")]))
        s.srv.enqueue(CannedChat(content="inner result"))  # inner gen
        s.generate("conv", tool_context="tooldesc")

        # Requests captured: outer (the act call), then inner (tooldesc.<id>).
        # The INNER call is the one whose first message is the tooldesc system.
        inner_req = next(
            r for r in s.srv.requests
            if r.messages and r.messages[0]["role"] == "system"
            and r.messages[0]["content"].split("\n", 1)[1] == "ALL TOOLS HERE")
        roles = [m["role"] for m in inner_req.messages]
        check("inner call prepends the tooldesc system before the user turn",
              roles == ["system", "user"])
        check("inner call's user turn is the request",
              inner_req.messages[1]["content"].split("\n", 1)[1] == "use a tool")
    finally:
        s.close()


# ── 30. follow_up continues an existing sub-agent ────────────────────────────
def test_tools_follow_up():
    s = Session()
    try:
        s.write_turn("conv", "0010.user", "please do X")
        # First happy-path call producing sub-agent H.
        s.srv.enqueue(CannedChat(
            tool_calls=[("act", '{"request": "search the thing"}', "call_1")]))
        s.srv.enqueue(CannedChat(content="RESULT1"))
        s.generate("conv")
        conv = oaita.session_dir("conv")
        callfile = sorted(p.name for p in conv.iterdir()
                          if p.name.endswith(".assistant"))[0]
        handle = _id_from_name(callfile)

        # Second round: the model follows up on H.
        s.srv.enqueue(CannedChat(tool_calls=[(
            "act", json.dumps({"request": "and also Y", "follow_up": handle}),
            "call_2")]))
        s.srv.enqueue(CannedChat(content="RESULT2"))
        s.generate("conv")

        inner = oaita.session_dir(handle)
        check("inner H now has two user + two assistant turns",
              _types_in_order(inner) ==
              ["user", "assistant", "user", "assistant"])
        icontents = _contents_in_order(inner)
        check("appended follow-up user turn is 'and also Y'",
              icontents[2] == "and also Y")
        check("new inner assistant turn is RESULT2", icontents[3] == "RESULT2")

        # A NEW outer .tool turn holds RESULT2; inner was NOT duplicated.
        tool_contents = [p.read_text() for p in conv.iterdir()
                         if p.name.endswith(".tool")]
        check("a new outer .tool turn holds RESULT2", "RESULT2" in tool_contents)
        sibling_dirs = [d.name for d in oaita.state_home().iterdir()
                        if d.is_dir()]
        check("the inner session was not duplicated",
              sibling_dirs.count(handle) == 1)
    finally:
        s.close()


# ── 31. always-on: the `act` tool is offered by default; plain reply is 1 turn ─
def test_tools_always_on_plain_reply():
    s = Session()
    try:
        s.write_turn("conv", "0010.user", "hi")
        s.srv.enqueue(CannedChat(content="plain reply"))
        produced = s.generate("conv")  # no flag — act is always offered
        check("the act tool IS offered by default (always-on)",
              s.srv.requests[-1].json["tools"][0]["function"]["name"] == "act")
        check("a content-only reply yields exactly one assistant turn",
              len(produced) == 1 and produced[0].name.endswith(".assistant"))
        check("plain reply stored raw", produced[0].read_text() == "plain reply")
    finally:
        s.close()


# ── 32. gen stops at the tool result; the user drives the next step ──────────
def test_tools_stops_after_result():
    s = Session()
    try:
        s.write_turn("conv", "0010.user", "what is X")
        s.srv.enqueue(CannedChat(
            tool_calls=[("act", '{"request": "look up X"}', "c1")]))
        s.srv.enqueue(CannedChat(content="X is 42"))  # inner sub-agent result
        produced = s.generate("conv")
        conv = oaita.session_dir("conv")

        check("gen stops at the tool result (turns: user, assistant, tool)",
              _types_in_order(conv) == ["user", "assistant", "tool"])
        check("exactly the call + result were produced", len(produced) == 2)
        # No auto-continuation: exactly one OUTER request + one INNER request.
        # A re-run of the main model would show up as a 3rd captured request.
        check("no auto-continuation: exactly two model requests made",
              len(s.srv.requests) == 2)

        # The user drives the next step: a second gen now reacts to the result.
        s.srv.enqueue(CannedChat(content="The answer is 42."))
        produced2 = s.generate("conv")
        check("second gen appends one synthesised assistant turn",
              len(produced2) == 1 and produced2[0].name.endswith(".assistant"))
        check("synthesis (reacting to the result) is the new tail",
              _contents_in_order(conv)[-1] == "The answer is 42.")
    finally:
        s.close()


# ── 33. several act calls in one model output are all evaluated, then stop ────
def test_tools_multiple_calls_one_gen():
    s = Session()
    try:
        s.write_turn("conv", "0010.user", "do A and B")
        s.srv.enqueue(CannedChat(tool_calls=[
            ("act", '{"request": "do A"}', "cA"),
            ("act", '{"request": "do B"}', "cB"),
        ]))
        s.srv.enqueue(CannedChat(content="result A"))  # inner for call A
        s.srv.enqueue(CannedChat(content="result B"))  # inner for call B
        produced = s.generate("conv")
        conv = oaita.session_dir("conv")

        check("both calls evaluated then stop: assistant,tool,assistant,tool",
              _types_in_order(conv)[1:] ==
              ["assistant", "tool", "assistant", "tool"])
        contents = _contents_in_order(conv)
        check("call A request + result recorded",
              contents[1] == "do A" and contents[2] == "result A")
        check("call B request + result recorded",
              contents[3] == "do B" and contents[4] == "result B")
        check("four turns produced (two call+result pairs)", len(produced) == 4)

        callslugs = [_id_from_name(p.name) for p in sorted(conv.iterdir())
                     if p.name.endswith(".assistant")]
        check("two distinct sub-agent sessions exist, one per call",
              len(set(callslugs)) == 2
              and all(oaita.session_dir(h).is_dir() for h in callslugs))
    finally:
        s.close()


# ── 34. deterministic turn-ids under $OAITA_ID_SEED ──────────────────────────
def test_deterministic_ids_with_seed():
    """Same folder state + same seed → identical ids; seed change → different;
    collisions probe deterministically (uniqueness from `existing`, not luck)."""
    def run_once(seed):
        s = Session()
        os.environ["OAITA_ID_SEED"] = seed
        try:
            name = "det"
            s.write_turn(name, "0010.user", "hello")
            s.srv.enqueue(CannedChat(content="hi"))
            s.generate(name)
            return sorted(p.name for p in oaita.session_dir(name).iterdir())
        finally:
            os.environ.pop("OAITA_ID_SEED", None)
            s.close()

    a, b, c = run_once("s1"), run_once("s1"), run_once("s2")
    check("same seed reproduces the exact same filenames", a == b)
    check("a different seed yields different ids", a != c)
    check("deterministic ids still match the id shape",
          all(_ID_RE.match(_id_from_name(f)) for f in a))

    # Pure-function probe: an occupied slot's first candidate forces probe #1,
    # and the result is itself stable.
    os.environ["OAITA_ID_SEED"] = "s1"
    try:
        first = oaita.new_turn_id(set(), slot="det/10")
        probed = oaita.new_turn_id({first}, slot="det/10")
        probed2 = oaita.new_turn_id({first}, slot="det/10")
        check("collision probes to a new id", probed != first)
        check("the probed id is itself deterministic", probed == probed2)
        no_slot = oaita.new_turn_id({first})
        check("no slot falls back to the random path (valid, unique id)",
              bool(_ID_RE.match(no_slot)) and no_slot != first)
    finally:
        os.environ.pop("OAITA_ID_SEED", None)


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
        test_strip_emitted_header_unit,
        test_append_adopts_model_id,
        test_append_rejects_duplicate_id,
        test_append_rejects_invalid_id,
        test_regenerate_strips_but_keeps_stable_id,
        test_stitch_prepends_and_targets_last,
        test_stitch_into_empty_target,
        test_stitch_reorder_changes_context_order,
        test_stitch_regenerate_only_target_tail,
        test_name_validation,
        test_stitch_cross_segment_unique_ids,
        test_stitch_turn_id_collision_guard,
        test_tools_happy_path,
        test_tools_capabilities_surfaced,
        test_tools_tool_context_stitched,
        test_tools_follow_up,
        test_tools_always_on_plain_reply,
        test_tools_stops_after_result,
        test_tools_multiple_calls_one_gen,
        test_deterministic_ids_with_seed,
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
