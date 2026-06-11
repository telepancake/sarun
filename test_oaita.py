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
  3. Continue/regenerate: a trailing PARTIAL (`p`-flagged) assistant turn is
     rewritten in place (no new file) and excluded from the prompt; its slug/id
     is stable; the p flag is dropped on completion. A clean tail appends.
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
 27. Tool calling happy path: gen persists the act call as a c-flagged envelope
     turn and STOPS; `call` evaluates it in the inner sub-agent session; the
     result turn carries from=<inner>, the inner seed carries from=<outer>.
 28. Capabilities surfaced: the advertised single tool is `act` and its
     description embeds the (custom or default) capabilities string.
 29. tool_context stitched: the inner gen's prompt prepends the tool-description
     system turn before the inner user turn.
 30. Follow-up: follow_up continues the existing sub-agent (appends user+assistant
     to it, not duplicated); the result's from-sender is the followed session.
 31. Always-on: the `act` tool is offered by default; a plain reply is one turn.
 32. gen refuses while a call is pending; `call` evaluates one; gen then reacts.
 33. Several act calls in one gen → several c-turns; `call` evaluates one per
     invocation; positional pairing routes each result to its call.
 35. pending_calls pairing rules (pure function).
 36. run drives gen → call → gen to a clean assistant tail, then no-ops.

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


def _load(name: str):
    """Load an extensionless module ONCE per process. load_module() RE-EXECUTES
    an already-loaded module (reload semantics), which forks class identities
    when several test files load the same module — isinstance checks then fail
    across files in a combined pytest run. Reuse sys.modules instead."""
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

    def call(self, name, **kw):
        return oaita.evaluate_call(
            name, model="test-model", base_url=self.srv.base_url,
            api_key="test-key", echo=lambda _text: None, **kw)

    def run(self, name, **kw):
        return oaita.run_to_completion(
            name, model="test-model", base_url=self.srv.base_url,
            api_key="test-key", echo=lambda _text: None, **kw)


# ── helpers ──────────────────────────────────────────────────────────────────
_ID_RE = re.compile(r"^[a-z]{5}$")


def _id_from_name(name: str) -> str | None:
    """Extract the turnid from a filename, e.g. '0020-xqvmb-SND.p.assistant' → 'xqvmb'."""
    mo = re.match(r"^\d+-([a-z0-9]+)", name)
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

    # Extended grammar: sender + flags fields.
    t3 = oaita.parse_turn(Path("0030-abcde-MAIN.tool"))
    check("turnid-from parses to (slug='abcde', sender='MAIN')",
          t3 is not None and (t3.slug, t3.sender, t3.flags) ==
          ("abcde", "MAIN", ""))
    t4 = oaita.parse_turn(Path("0040-abcde.p.assistant"))
    check("p flag parses (partial assistant)",
          t4 is not None and (t4.slug, t4.sender, t4.flags) ==
          ("abcde", None, "p"))
    t5 = oaita.parse_turn(Path("0050-abcde-sub1.pi.user"))
    check("combined flags + sender parse",
          t5 is not None and (t5.slug, t5.sender, t5.flags) ==
          ("abcde", "sub1", "pi"))
    check("unknown flag chars → not a turn (ignored)",
          oaita.parse_turn(Path("0060-abcde.zz.user")) is None)
    check("uppercase turnid → not a turn (turnid is [a-z0-9])",
          oaita.parse_turn(Path("0070-ABCDE.user")) is None)
    check("flags without slug parse too",
          oaita.parse_turn(Path("0080.p.assistant")).flags == "p")
    check("turn_filename round-trips sender+flags",
          oaita.turn_filename(30, "tool", "abcde", sender="MAIN", flags="p")
          == "0030-abcde-MAIN.p.tool")
    raised = False
    try:
        oaita.turn_filename(30, "tool", None, sender="MAIN")
    except ValueError:
        raised = True
    check("sender without slug is rejected", raised)

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


# ── 3. continue / regenerate in place (a `p`-flagged partial tail) ───────────
def test_regenerate_in_place():
    s = Session()
    try:
        name = "regen"
        s.write_turn(name, "0010.user", "hello")
        s.write_turn(name, "0020.p.assistant", "stale partial")  # interrupted
        s.srv.enqueue(CannedChat(content="regenerated"))
        produced = s.generate(name)
        folder = oaita.session_dir(name)

        # Both turns get slugs; the partial is regenerated and loses its p flag.
        files = sorted(p.name for p in folder.iterdir())
        check("exactly two turn files on disk after regen", len(files) == 2)
        check("user turn has slug", bool(re.match(r"^0010-[a-z]{5}\.user$", files[0])))
        check("assistant turn has slug and dropped the p flag",
              bool(re.match(r"^0020-[a-z]{5}\.assistant$", files[1])))
        check("original slug-less 0020.p.assistant gone",
              not (folder / "0020.p.assistant").exists())

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
        s.write_turn(name, "0030.p.assistant", "pong")
        # Last turn is a PARTIAL assistant → regenerate; prompt = system + user.
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


# ── 14. regenerate keeps id stable; a CLEAN tail is never rewritten ──────────
def test_regenerate_keeps_id_stable():
    """Regenerating a partial keeps its id (p dropped); a clean assistant tail
    is a finished answer — gen after it APPENDS, never rewrites."""
    s = Session()
    try:
        name = "stable"
        s.write_turn(name, "0010-hi.user", "hello")
        # Pre-write a slugged PARTIAL assistant tail.
        s.write_turn(name, "0020-oldid.p.assistant", "stale")
        s.srv.enqueue(CannedChat(content="fresh"))
        produced = s.generate(name)
        folder = oaita.session_dir(name)
        # Same slug, p flag dropped on completion.
        check("regenerated file keeps original slug 'oldid' (p dropped)",
              produced[0].name == "0020-oldid.assistant")
        check("the p-flagged name is gone",
              not (folder / "0020-oldid.p.assistant").exists())
        check("content overwritten to new reply",
              produced[0].read_text() == "fresh")
        # The tail is now CLEAN → a further gen appends; oldid stays untouched.
        s.srv.enqueue(CannedChat(content="more"))
        produced2 = s.generate(name)
        check("gen after a clean tail APPENDS a new 0030 turn",
              bool(re.match(r"^0030-[a-z]{5}\.assistant$", produced2[0].name)))
        check("the clean tail was not rewritten",
              (folder / "0020-oldid.assistant").read_text() == "fresh")
        # The appended generation's prompt INCLUDED the clean assistant turn.
        req = s.srv.requests[-1]
        check("clean assistant tail included in the follow-on prompt",
              [m["role"] for m in req.messages] == ["user", "assistant"])
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
    check("a from-bearing header is stripped (from never adopted)",
          f('{"turn-id": "abcde", "from": "MAIN"}\nbody') == ("abcde", "body"))


# ── 15b. from + flags surface in the injected header / suppress it ───────────
def test_header_from_and_iflag():
    s = Session()
    try:
        name = "fromhdr"
        s.write_turn(name, "0010-seed1-OTHER.user", "posted by OTHER")
        s.write_turn(name, "0020-quiet1.i.user", "no header for me")
        s.write_turn(name, "0030-mine1.user", "own turn")
        s.srv.enqueue(CannedChat(content="ok"))
        s.generate(name)
        msgs = s.srv.requests[-1].messages
        check("sender surfaces as from in the injected header",
              msgs[0]["content"] ==
              '{"turn-id": "seed1", "from": "OTHER"}\nposted by OTHER')
        check("i flag suppresses the header entirely",
              msgs[1]["content"] == "no header for me")
        check("own turn header has no from key",
              msgs[2]["content"] == '{"turn-id": "mine1"}\nown turn')
    finally:
        s.close()


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
        s.write_turn(name, "0020-oldid.p.assistant", "stale")
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
        # ttt now ends with a CLEAN assistant turn (kept); reorder x/y — the
        # next gen appends, so the prompt includes the r1 tail.
        s.srv.enqueue(CannedChat(content="r2"))
        s.generate("yyy.xxx.ttt")
        check("y.x.t context order is [Y, X, go, r1]",
              _bodies(s.srv.requests[-1].messages) ==
              ["from Y", "from X", "go", "r1"])
    finally:
        s.close()


# ── 23. regenerate touches ONLY the target segment's tail ────────────────────
def test_stitch_regenerate_only_target_tail():
    s = Session()
    try:
        s.write_turn("sys", "0010.system", "sys prompt")
        s.write_turn("conv", "0010.user", "q")
        s.write_turn("conv", "0020.p.assistant", "old answer")
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


# ── 27. tool calling happy path: gen persists the call, `call` evaluates ─────
def test_tools_happy_path():
    s = Session()
    try:
        s.write_turn("conv", "0010.user", "please do X")
        # gen: the model calls act → ONE c-flagged call turn persisted, nothing
        # evaluated, exactly one wire request.
        s.srv.enqueue(CannedChat(
            tool_calls=[("act", '{"request": "search the thing"}', "call_1")]))
        produced = s.generate("conv")
        conv = oaita.session_dir("conv")
        check("gen persisted the call and STOPPED: turns are user, assistant",
              _types_in_order(conv) == ["user", "assistant"])
        check("gen made exactly one wire request", len(s.srv.requests) == 1)
        check("gen returned the single call turn", len(produced) == 1)
        call_turn = oaita.parse_turn(produced[0])
        check("the call turn carries the c flag", call_turn.flags == "c")
        env = json.loads(produced[0].read_text())
        check("call turn content is the envelope (tool + parsed arguments)",
              env == {"tool": "act",
                      "arguments": {"request": "search the thing"}})

        # call: evaluates the pending call in the inner sub-agent session.
        s.srv.enqueue(CannedChat(content="THE RESULT"))
        res = s.call("conv")
        check("call wrote exactly the result turn", len(res) == 1)
        check("turns now user, assistant(call), tool",
              _types_in_order(conv) == ["user", "assistant", "tool"])
        result_turn = oaita.parse_turn(res[0])
        check("result content is exactly the inner answer",
              res[0].read_text() == "THE RESULT")
        handle = call_turn.slug
        check("result turn's from-sender is the inner session (the call's id)",
              result_turn.sender == handle)

        inner = oaita.session_dir(handle)
        check("inner sub-agent session exists named after the call slug",
              inner.is_dir())
        check("inner session has a .user(request) then .assistant(result)",
              _types_in_order(inner) == ["user", "assistant"])
        icontents = _contents_in_order(inner)
        check("inner user turn is the request", icontents[0] == "search the thing")
        check("inner assistant turn is the result", icontents[1] == "THE RESULT")
        inner_user = [oaita.parse_turn(p) for p in sorted(inner.iterdir())
                      if p.name.endswith(".user")][0]
        check("inner seed turn's from-sender is the OUTER session",
              inner_user.sender == "conv")
        # The inner gen saw the from-bearing header on the wire.
        inner_req = s.srv.requests[-1]
        check("inner prompt's user turn carries from=conv in its header",
              '"from": "conv"' in inner_req.messages[0]["content"])
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
        by_name = {t["function"]["name"]: t["function"] for t in tools}
        check("the registry tools are advertised (act + shell)",
              set(by_name) == {"act", "shell"})
        check("custom capabilities embedded in the act description",
              "bespoke power" in by_name["act"]["description"])
        check("shell schema requires the script param",
              by_name["shell"]["parameters"]["required"] == ["script"])

        # And the default capabilities surface when none is passed.
        s.srv.enqueue(CannedChat(content="answer2"))
        s.write_turn("conv2", "0010.user", "do Y")
        s.generate("conv2")
        fns = {t["function"]["name"]: t["function"]
               for t in s.srv.requests[-1].json["tools"]}
        check("default capabilities embedded when none passed",
              oaita.DEFAULT_CAPABILITIES in fns["act"]["description"])
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
        s.generate("conv")
        s.srv.enqueue(CannedChat(content="inner result"))  # inner gen
        s.call("conv", tool_context="tooldesc")

        # The INNER request is the one whose first message is the tooldesc system.
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
        # First round: call producing sub-agent H.
        s.srv.enqueue(CannedChat(
            tool_calls=[("act", '{"request": "search the thing"}', "call_1")]))
        s.generate("conv")
        conv = oaita.session_dir("conv")
        s.srv.enqueue(CannedChat(content="RESULT1"))
        s.call("conv")
        callfile = sorted(p.name for p in conv.iterdir()
                          if p.name.endswith(".assistant"))[0]
        handle = _id_from_name(callfile)

        # Second round: the model follows up on H (the result's from-sender).
        s.srv.enqueue(CannedChat(tool_calls=[(
            "act", json.dumps({"request": "and also Y", "follow_up": handle}),
            "call_2")]))
        s.generate("conv")
        s.srv.enqueue(CannedChat(content="RESULT2"))
        res2 = s.call("conv")

        inner = oaita.session_dir(handle)
        check("inner H now has two user + two assistant turns",
              _types_in_order(inner) ==
              ["user", "assistant", "user", "assistant"])
        icontents = _contents_in_order(inner)
        check("appended follow-up user turn is 'and also Y'",
              icontents[2] == "and also Y")
        check("new inner assistant turn is RESULT2", icontents[3] == "RESULT2")

        # A NEW outer .tool turn holds RESULT2, from=H; inner NOT duplicated.
        check("a new outer .tool turn holds RESULT2",
              res2[0].read_text() == "RESULT2")
        check("the follow-up result's from-sender is H (not the new call id)",
              oaita.parse_turn(res2[0]).sender == handle)
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
              "act" in [t["function"]["name"]
                        for t in s.srv.requests[-1].json["tools"]])
        check("a content-only reply yields exactly one assistant turn",
              len(produced) == 1 and produced[0].name.endswith(".assistant"))
        check("plain reply stored raw", produced[0].read_text() == "plain reply")
    finally:
        s.close()


# ── 32. gen refuses a pending call; call evaluates; the caller drives ─────────
def test_tools_stops_after_result():
    s = Session()
    try:
        s.write_turn("conv", "0010.user", "what is X")
        s.srv.enqueue(CannedChat(
            tool_calls=[("act", '{"request": "look up X"}', "c1")]))
        produced = s.generate("conv")
        conv = oaita.session_dir("conv")
        check("gen persisted ONE call turn and made ONE request",
              len(produced) == 1 and len(s.srv.requests) == 1)

        # gen REFUSES to run while the call is unanswered.
        raised = False
        try:
            s.generate("conv")
        except SystemExit:
            raised = True
        check("gen refuses while a call is pending", raised)

        # call evaluates it (one inner request).
        s.srv.enqueue(CannedChat(content="X is 42"))
        s.call("conv")
        check("turns now user, assistant(call), tool",
              _types_in_order(conv) == ["user", "assistant", "tool"])
        check("call made exactly one further request",
              len(s.srv.requests) == 2)

        # The caller drives the next step: gen now reacts to the result.
        s.srv.enqueue(CannedChat(content="The answer is 42."))
        produced2 = s.generate("conv")
        check("next gen appends one synthesised assistant turn",
              len(produced2) == 1 and produced2[0].name.endswith(".assistant"))
        check("synthesis (reacting to the result) is the new tail",
              _contents_in_order(conv)[-1] == "The answer is 42.")
    finally:
        s.close()


# ── 33. several act calls in one gen; calls evaluate in order ─────────────────
def test_tools_multiple_calls_one_gen():
    s = Session()
    try:
        s.write_turn("conv", "0010.user", "do A and B")
        s.srv.enqueue(CannedChat(tool_calls=[
            ("act", '{"request": "do A"}', "cA"),
            ("act", '{"request": "do B"}', "cB"),
        ]))
        produced = s.generate("conv")
        conv = oaita.session_dir("conv")
        check("gen persisted BOTH calls as c-turns (one wire request)",
              len(produced) == 2 and len(s.srv.requests) == 1
              and _types_in_order(conv) == ["user", "assistant", "assistant"])
        check("both call turns carry the c flag",
              all("c" in oaita.parse_turn(p).flags for p in produced))

        # Evaluate one call per `call` invocation; pairing is positional.
        s.srv.enqueue(CannedChat(content="result A"))
        resA = s.call("conv")
        s.srv.enqueue(CannedChat(content="result B"))
        resB = s.call("conv")
        check("results landed in call order",
              resA[0].read_text() == "result A"
              and resB[0].read_text() == "result B")
        check("turn shape: user, call, call, tool, tool",
              _types_in_order(conv) ==
              ["user", "assistant", "assistant", "tool", "tool"])
        slugA, slugB = (_id_from_name(p.name) for p in produced)
        check("result A's sender is call A's id; B's is B's",
              oaita.parse_turn(resA[0]).sender == slugA
              and oaita.parse_turn(resB[0]).sender == slugB)
        check("two distinct sub-agent sessions exist, one per call",
              slugA != slugB
              and oaita.session_dir(slugA).is_dir()
              and oaita.session_dir(slugB).is_dir())
        check("inner sessions hold their own requests",
              _contents_in_order(oaita.session_dir(slugA))[0] == "do A"
              and _contents_in_order(oaita.session_dir(slugB))[0] == "do B")

        # No pending call remains.
        raised = False
        try:
            s.call("conv")
        except SystemExit:
            raised = True
        check("a third call raises (nothing pending)", raised)
    finally:
        s.close()


# ── 35. pending_calls pairing (pure function) ─────────────────────────────────
def test_pending_calls_unit():
    def turn(num, type, flags=""):
        return oaita.Turn(number=num, slug=f"t{num:02d}", type=type,
                          path=Path(f"{num:04d}-t{num:02d}.{type}"), flags=flags)

    f = oaita.pending_calls
    check("no turns → no pending", f([]) == [])
    check("plain tail → no pending", f([turn(10, "user")]) == [])
    one = [turn(10, "user"), turn(20, "assistant", flags="c")]
    check("one unanswered call pending", [t.number for t in f(one)] == [20])
    answered = one + [turn(30, "tool")]
    check("answered call → nothing pending", f(answered) == [])
    two = [turn(10, "user"), turn(20, "assistant", flags="c"),
           turn(30, "assistant", flags="c")]
    check("two calls, no results → both pending, in order",
          [t.number for t in f(two)] == [20, 30])
    check("two calls, one result → second pending (positional pairing)",
          [t.number for t in f(two + [turn(40, "tool")])] == [30])
    older = [turn(10, "assistant", flags="c"), turn(20, "user"),
             turn(30, "assistant", flags="c")]
    check("the block stops at a non-call/result turn (old calls settled)",
          [t.number for t in f(older)] == [30])
    clean = [turn(10, "user"), turn(20, "assistant")]
    check("a clean assistant tail is not a call", f(clean) == [])


# ── 36. run drives gen → call → gen to a clean answer ─────────────────────────
def test_run_to_completion():
    s = Session()
    try:
        s.write_turn("conv", "0010.user", "what is X")
        s.srv.enqueue(CannedChat(
            tool_calls=[("act", '{"request": "look up X"}', "c1")]))
        s.srv.enqueue(CannedChat(content="X is 42"))          # inner leaf
        s.srv.enqueue(CannedChat(content="The answer is 42."))  # synthesis
        produced = s.run("conv")
        conv = oaita.session_dir("conv")
        check("run settled: user, call, tool, assistant",
              _types_in_order(conv) == ["user", "assistant", "tool", "assistant"])
        check("the final tail is the clean synthesised answer",
              _contents_in_order(conv)[-1] == "The answer is 42.")
        check("exactly three wire requests (gen, inner, gen)",
              len(s.srv.requests) == 3)
        check("run produced call, result, answer (3 written turns)",
              len(produced) == 3)
        # A settled context is a no-op.
        check("run on a settled context does nothing", s.run("conv") == [])
        check("no extra requests for the no-op", len(s.srv.requests) == 3)
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


# ── 36b. the shell tool: executor interface, box naming, error mode ──────────
class FakeExecutor:
    """Scripted stand-in for SarunExecutor: canned results + a call log."""

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


def test_shell_tool():
    s = Session()
    try:
        s.write_turn("conv", "0010.user", "fix the build")
        s.srv.enqueue(CannedChat(tool_calls=[
            ("shell", '{"script": "make 2>&1 | tail -3"}', "c1")]))
        s.generate("conv")
        ex = FakeExecutor().stage(
            "cc -o app main.c\nmain.c:7: error: missing ;",
            changes="1 file(s) changed (staged in the box):\nbuild.log: +3 -0",
            exit_code=2)
        res = s.call("conv", executor=ex)
        check("the executor received the script and the session box",
              ex.calls == [("make 2>&1 | tail -3", "OAITA-CONV")])
        body = res[0].read_text()
        check("result carries exit code, output and change summary",
              body.startswith("exit 2\n") and "missing ;" in body
              and "build.log: +3 -0" in body)
        check("a shell result has no from-sender (no sub-session)",
              oaita.parse_turn(res[0]).sender is None)

        # A second shell call reuses the SAME persistent box.
        s.srv.enqueue(CannedChat(tool_calls=[
            ("shell", '{"script": "ls"}', "c2")]))
        s.generate("conv")
        ex.stage("README", changes="")
        res2 = s.call("conv", executor=ex)
        check("follow-up shell call lands in the same box",
              ex.calls[-1][1] == "OAITA-CONV")
        check("a clean run reports no file changes",
              "[no file changes]" in res2[0].read_text())

        # No executor → an error result, and the loop keeps moving.
        s.srv.enqueue(CannedChat(tool_calls=[
            ("shell", '{"script": "rm -rf /"}', "c3")]))
        s.generate("conv")
        res3 = s.call("conv")          # executor=None
        check("no executor → error-text result; nothing was run",
              res3[0].read_text().startswith("error: no executor")
              and len(ex.calls) == 2)
    finally:
        s.close()


def test_summarize_patch_unit():
    f = oaita.summarize_patch
    check("empty patch → no summary", f("") == "")
    patch = ("--- a/x.py\n+++ b/x.py\n@@ -1,2 +1,3 @@\n line\n+added\n"
             "+added2\n-gone\n"
             "--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+hello\n")
    out = f(patch)
    check("per-file counts summarized",
          "x.py: +2 -1" in out and "new.txt: +1 -0" in out)
    check("file count stated", out.startswith("2 file(s) changed"))


# ── 37. add_turn: stdin-to-turn convenience, defaults and overrides ──────────
def test_add_turn():
    import io
    s = Session()
    try:
        # All defaults: type user, next number, generated slug.
        p1 = oaita.add_turn("addsess", source=io.BytesIO(b"hello there"))
        check("default add is 0010-<id>.user",
              bool(re.match(r"^0010-[a-z]{5}\.user$", p1.name)))
        check("content copied verbatim", p1.read_text() == "hello there")

        # Defaults stack on the grid: next add lands at 0020.
        p2 = oaita.add_turn("addsess", source=io.BytesIO(b"again"))
        check("second add is 0020-<id>.user",
              bool(re.match(r"^0020-[a-z]{5}\.user$", p2.name)))

        # Everything overridden, incl. insertion between 10 and 20.
        p3 = oaita.add_turn(
            "addsess", type="system", slug="rules", sender="OTHER",
            flags="i", number=15, source=io.BytesIO(b"be terse"))
        check("overridden add renders every field",
              p3.name == "0015-rules-OTHER.i.system")
        check("inserted turn sorts between its neighbours",
              [t.number for t in oaita.load_turns("addsess")] == [10, 15, 20])

        # Stitch spec targets the LAST segment.
        p4 = oaita.add_turn("addsess.tail", source=io.BytesIO(b"x"))
        check("stitch spec adds to the last segment",
              p4.parent.name == "tail")

        # Refusals: bad type / slug / sender / flags, and overwrite.
        for kw in (dict(type="bogus"), dict(slug="UPPER"),
                   dict(sender="not.aname"), dict(flags="zq"),
                   dict(slug="rules", number=15, type="system",
                        sender="OTHER", flags="i")):  # exact existing path
            raised = False
            try:
                oaita.add_turn("addsess", source=io.BytesIO(b""), **kw)
            except SystemExit:
                raised = True
            check(f"add_turn rejects {kw}", raised)
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
        test_strip_emitted_header_unit,
        test_header_from_and_iflag,
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
        test_pending_calls_unit,
        test_run_to_completion,
        test_shell_tool,
        test_summarize_patch_unit,
        test_deterministic_ids_with_seed,
        test_add_turn,
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
