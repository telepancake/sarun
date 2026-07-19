"""libtestsarun — test-support library carved from the old Python prototype.

The sarun engine (Rust) is the program; this file is NOT runnable. It holds
only the helpers the engine tests import: the wire client (sync_request,
RemoteSupervisor), the sqlar storage readers/writers used to build box
fixtures (Index, consolidate, sqlar_*), and the rules/hunks parity helpers."""

import abc

import array

import asyncio

import base64

import difflib

import functools

import heapq as _heapq

import hashlib

import json

try:
    import magic   # python-magic; used by the file-type / struct helpers
except ImportError:
    magic = None

import os

import re

import resource

import select

import shutil

import signal

import socket

import sqlite3

import struct

import stat as stat_mod

import subprocess

import sys

import tempfile

import threading

import time

import uuid

import zlib

from dataclasses import dataclass, field, asdict

from pathlib import Path

from typing import Any

def namespace() -> str:
    """The instance namespace, from $SLOPBOX_NS (set by the `--ns` CLI flag, or
    inherited: a box's processes get the runner's environment, so a nested runner
    lands in its parent's namespace by default). "" = the default instance."""
    return os.environ.get("SLOPBOX_NS", "")

def _app_dir() -> str:
    """The per-instance app dirname: every storage/config/runtime root (and thus
    the control socket, mountpoint, state, pool, rules) hangs off this one name,
    so two namespaces are fully independent instances on the same system."""
    ns = namespace()
    return f"slopbox.{ns}" if ns else "slopbox"

def data_home() -> Path:
    base = os.environ.get("XDG_DATA_HOME") or os.path.expanduser("~/.local/share")
    return Path(base) / _app_dir()

def config_home() -> Path:
    base = os.environ.get("XDG_CONFIG_HOME") or os.path.expanduser("~/.config")
    return Path(base) / _app_dir()

def runtime_home() -> Path:
    base = os.environ.get("XDG_RUNTIME_DIR")
    return Path(base) / _app_dir() if base else data_home()

def live_home() -> Path:
    """Root of per-box backing dirs: live/<box_id>/up is the discovery sentinel (an
    empty dir that gates box discovery — box writes do NOT land here). Sits next to
    the per-box <box_id>.sqlar under state_home — i.e. ON DISK, not the RAM-backed
    XDG_RUNTIME_DIR tmpfs. Being on the same filesystem as the lower (/) lets pool
    blobs reflink at copy-up. (runtime_home keeps only small ephemera: the control
    socket and the FUSE mountpoint.) Created mode 0700."""
    return state_home() / "live"

def live_dir(box_id) -> Path:
    # Keyed by the box's stable integer id (as a path component, str(box_id)). A
    # rename changes only the box's NAME label in meta — never this dir.
    return live_home() / str(box_id)

POOL_SHARDS = 1024

POOL_RESIDENT_MIN = 64 << 10

def pool_home() -> Path:
    return live_home() / "blob"

def box_pool_dir(box_id: int) -> Path:
    return pool_home() / str(int(box_id))

def blob_path(box_id: int, row_id: int) -> Path:
    rid = int(row_id)
    return box_pool_dir(box_id) / f"{rid % POOL_SHARDS:03x}" / str(rid)

def state_home() -> Path:
    base = os.environ.get("XDG_STATE_HOME") or os.path.expanduser("~/.local/state")
    return Path(base) / _app_dir()

def box_ctime(box_id) -> float:
    """A box's age anchor: the ctime of its on-disk identity — the sqlar if present,
    else the backing dir. Replaces parsing a timestamp out of the (now numeric) id."""
    for p in (sqlar_path(box_id), live_dir(box_id)):
        try: return os.stat(p).st_ctime
        except OSError: pass
    return 0.0

BOX_ID_RE = re.compile(r"\d+\Z")

NAME_RE = re.compile(r"[A-Z]([A-Z0-9-]*[A-Z0-9])?\Z")

DOTTED_NAME_RE = re.compile(
    r"[A-Z]([A-Z0-9-]*[A-Z0-9])?(\.[A-Z]([A-Z0-9-]*[A-Z0-9])?)* \Z".replace(" ", ""))

def valid_name(name: "str | None") -> bool:
    return bool(isinstance(name, str) and NAME_RE.match(name))

def valid_dotted_name(name: "str | None") -> bool:
    """True for a dotted-path name: one or more NAME segments joined by '.'.
    Each segment matches NAME_RE (starts with a capital letter, only CAPS/digits/'-',
    never ends with '-').  Rejects empty string, leading/trailing dots, consecutive dots
    ('..'), any '/', and any standalone '..' segment — all traversal-safe by construction.
    A single-segment name (no dot) is also valid (it equals valid_name)."""
    return bool(isinstance(name, str) and DOTTED_NAME_RE.match(name))

def valid_box_id(box_id: "str | None") -> bool:
    # The internal box key is str(box_id): a plain decimal string. This validates a
    # path component / teardown key — NOT a user-facing name (see valid_name).
    return bool(isinstance(box_id, str) and BOX_ID_RE.match(box_id))

SINK_STDOUT_REL = ".slopbox-stdout"

SINK_STDERR_REL = ".slopbox-stderr"

def mnt_point() -> Path:
    """The single pyfuse3 mountpoint the UI owns; box subfolders live under it as
    <mnt>/<box_id>. Empty at rest."""
    return runtime_home() / "mnt"

def sock_path() -> str:
    """Unix socket path for the UI's control channel."""
    return str(runtime_home() / "ui.sock")

def file_rules_file() -> Path:
    """Path to the file apply/discard glob-rules file."""
    return config_home() / "filerules"

def ensure_dirs() -> None:
    """Create our host-side config/data/runtime directories. Called by the UI and
    the runner — never by the in-slopbox --inner process."""
    for p in (data_home(), config_home(), runtime_home(), state_home()):
        try: p.mkdir(parents=True, exist_ok=True)
        except OSError: pass
    # live/ holds provenance (pid/exe/argv of writers) — restrict it.
    try: live_home().mkdir(parents=True, mode=0o700, exist_ok=True)
    except OSError: pass
    try: os.chmod(live_home(), 0o700)
    except OSError: pass
    try: mnt_point().mkdir(parents=True, exist_ok=True)
    except OSError: pass
    try: file_rules_file().touch(exist_ok=True)
    except OSError: pass

def sync_request(sock_path: str, timeout: float = 30.0, **msg: Any) -> "dict | None":
    """Send one control message and wait for a single newline-JSON reply. Used by the
    runner to register a box and block until the UI has created live/<box_id> and
    exposed <mnt>/<box_id> (so the bwrap bind target exists). None on any failure."""
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
            s.settimeout(timeout); s.connect(sock_path)
            s.sendall((json.dumps(msg) + "\n").encode())
            line = s.makefile("rb").readline()
            return json.loads(line.decode()) if line.strip() else None
    except (OSError, json.JSONDecodeError):
        return None

def ui_is_running(sock_path: str) -> bool:
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
            s.settimeout(1.0); s.connect(sock_path); return True
    except OSError:
        return False

SUBJECT_KINDS = ("box", "exe", "cwd", "arg")

FILE_KINDS = ("path",) + SUBJECT_KINDS

@functools.cache
def _glob_flags() -> int:
    from wcmatch import glob as _wg
    return _wg.GLOBSTAR | _wg.EXTGLOB | _wg.BRACE | _wg.DOTGLOB

def _glob_match(pat: str, s: str) -> bool:
    """True if `s` matches the extended shell glob `pat`. Empty pattern → False."""
    pat = pat.strip()
    if not pat: return False
    from wcmatch import glob as _wg
    try:
        return bool(_wg.globmatch(s, pat, flags=_glob_flags()))
    except ValueError:
        return False

@dataclass
class Match:
    """The ATOMIC predicate — one kind + its pattern — and the only thing an object
    knows how to test against itself. The leaf of a rule's match expression; the
    and/or/not/enabled composition over a list of these lives generically on the rule
    (Clause + eval_clauses), and what each kind means is decided solely by the target
    (PathTarget.match_one). 'box' is just another kind: a glob over the box's
    hierarchical display name, so a single rule can require both a path facet AND a
    box."""
    kind: str
    pattern: str

@dataclass
class Clause:
    """One line of a rule's match list: a Match plus how it composes. `join` folds it
    into the running result (left to right — the first enabled clause seeds it, its
    join ignored); `negate` flips this one predicate; `enabled` skips it while keeping
    it for round-trip, so a condition can be toggled off without deleting it."""
    match: Match
    join: str = "and"          # "and" | "or"
    negate: bool = False
    enabled: bool = True

def eval_clauses(target, clauses) -> bool:
    """Generic, TARGET-AGNOSTIC boolean fold over a rule's clause list. Each enabled
    clause asks the target one yes/no question (target.match_one), optionally negates
    it, and folds it into the accumulator with its and/or join — left to right, no
    precedence (what the flat clause-list UI shows is what you get). A rule with no
    enabled clause matches nothing (False), never everything."""
    acc = None
    for c in clauses:
        if not c.enabled:
            continue
        v = target.match_one(c.match)
        if c.negate:
            v = not v
        if acc is None:
            acc = v
        elif c.join == "or":
            acc = acc or v
        else:
            acc = acc and v
    return bool(acc)

@dataclass
class Subject:
    """The box + the triggering PROCESS a match is evaluated against — the context
    shared by every domain. For a file rule it's the path's FIRST writer (the process
    whose decision the path is locked to). An empty field simply never matches its
    kind. New process-facets are cheap to add here without touching any rule code."""
    box: str = ""
    exe: str = ""          # the process's command pathname
    cwd: str = ""          # its working directory
    argv: tuple = ()       # its argument vector

def _subject_match(m: Match, s: Subject) -> bool:
    """match_one for the kinds every domain shares — the box and the triggering
    process. The single place these live, so every rule kind tests them identically
    and a new facet is one line. exe/cwd are pathnames, matched with the
    same path-glob semantics as file paths (a bare pattern matches at any depth, a
    leading / anchors); box and argv are plain strings matched with the raw glob."""
    k, p = m.kind, m.pattern
    if k == "box": return _glob_match(p, s.box)
    if k == "exe": return _path_match(p, s.exe)
    if k == "cwd": return _path_match(p, s.cwd)
    if k == "arg": return any(_glob_match(p, a) for a in (s.argv or ()))
    return False

def _subject_of(box="", proc=None) -> Subject:
    """Build a Subject from a box name + a process provenance dict (exe/cwd/argv)."""
    proc = proc or {}
    return Subject(box=box or "", exe=proc.get("exe") or "",
                   cwd=proc.get("cwd") or "", argv=tuple(proc.get("argv") or ()))

def _ids_of(pattern: str) -> "set[int]":
    """Parse the INTERNAL "ids" kind's pattern — a comma-separated list of process ROW
    ids (e.g. "5,7") — into a set. Non-numeric / empty fields are skipped. Only ever
    built programmatically (cross-pane navigation), never typed: an "ids" Match asks a
    target "is this entry one of these rows?", so a list view can be pinned to exactly
    the processes/files a navigation selected. Not a user kind (absent from every
    *_KINDS), so the search kind-picker never offers it."""
    out = set()
    for tok in (pattern or "").split(","):
        tok = tok.strip()
        if tok:
            try: out.add(int(tok))
            except ValueError: pass
    return out

@dataclass
class PathTarget:
    """A changed path under evaluation — the file-domain target.
    Tests the 'path' kind against its rel, then defers box/process kinds to the subject
    (a file rule's subject is the path's FIRST writer)."""
    rel: str = ""
    subject: Subject = field(default_factory=Subject)
    ids: tuple = ()            # the change's writer row id(s) — for the internal "ids" kind

    def match_one(self, m: Match) -> bool:
        if m.kind == "path":
            return _path_match(m.pattern, self.rel)
        if m.kind == "ids":
            return bool(set(self.ids) & _ids_of(m.pattern))
        return _subject_match(m, self.subject)

FILE_ACTIONS = ("apply", "discard", "passthrough")

@dataclass
class FileRule:
    """A file rule = an action (apply/discard/passthrough) + an ordered list of Clauses
    (the and/or/not/enabled match expression). Line grammar:

        ACTION [off] [not] PRED [and|or [off] [not] PRED]...

    where PRED is `kind:pattern` (or a bare pattern → the `path` kind, which is also the
    one kind that renders without its `kind:` prefix on disk). A one-predicate rule reads
    naturally — `discard **/*.log` — and a multi-condition rule chains clauses:
    `discard **/*.log and not box:trusted`. An explicit action is always required."""
    action: str
    clauses: list                       # list[Clause]

    def matches(self, target) -> bool:
        return eval_clauses(target, self.clauses)

    @classmethod
    def single(cls, action: str, kind: str, pattern: str) -> "FileRule":
        """A one-predicate rule — the common case (and what parse builds for a
        single-predicate line)."""
        return cls(action, [Clause(Match(kind, pattern))])

    def to_line(self) -> str:
        out = [self.action]
        for n, c in enumerate(self.clauses):
            seg = []
            if n: seg.append(c.join)
            if not c.enabled: seg.append("off")
            if c.negate: seg.append("not")
            k = c.match.kind
            seg.append(c.match.pattern if k == "path"      # the 'path' kind renders bare
                       else f"{k}:{c.match.pattern}")
            out.append(" ".join(seg))
        return " ".join(out)

    @staticmethod
    def _parse_clauses(s: str) -> list:
        toks = s.split()
        clauses = []; i = 0; join = "and"
        while i < len(toks):
            if clauses and toks[i].lower() in ("and", "or"):
                join = toks[i].lower(); i += 1
            off = neg = False
            while i < len(toks) and toks[i].lower() in ("off", "not"):
                if toks[i].lower() == "off": off = True
                else: neg = True
                i += 1
            if i >= len(toks): break
            pred = toks[i]; i += 1
            kind, sep, pat = pred.partition(":")
            if sep and kind.lower() in FILE_KINDS:
                kind = kind.lower()
            else:
                kind = "path"; pat = pred                # bare pattern → path kind
            if not pat: continue
            clauses.append(Clause(Match(kind, pat),
                                  join=(join if clauses else "and"),
                                  negate=neg, enabled=not off))
            join = "and"
        return clauses

    @classmethod
    def parse(cls, line: str) -> "FileRule | None":
        s = line.strip()
        if not s or s.startswith("#"):
            return None
        verb, _, rest = s.partition(" ")
        if verb.lower() not in FILE_ACTIONS:
            return None                                 # an explicit action is required
        action = verb.lower(); s = rest.strip()
        clauses = cls._parse_clauses(s)
        if not clauses:
            return None
        return cls(action, clauses)

def _path_match(pat: str, rel: str) -> bool:
    """Glob a change's ABSOLUTE path with the shared wcmatch engine. A bare/relative
    pattern matches at any depth (**/ prefix); a leading / anchors at the root."""
    pat = pat.strip()
    if not pat: return False
    s = "/" + rel.lstrip("/")                    # the change's absolute path
    if "/" not in pat or not pat.startswith("/"):
        pat = "**/" + pat                        # bare name / relative → any depth
    return _glob_match(pat, s)

class FileRules:
    """An ordered apply/discard/passthrough rule list (FileRule), persisted one rule per
    line and evaluated top-to-bottom — first match wins."""

    def __init__(self, path: Path):
        self.path = path; self.rules: list = []; self.load()
    def load(self) -> None:
        try: text = self.path.read_text()
        except OSError: text = ""
        self.rules = [r for r in (FileRule.parse(ln) for ln in text.splitlines()) if r]
    def save(self) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.path.write_text("".join(r.to_line() + "\n" for r in self.rules))
    def insert(self, rule, at_top: bool = False) -> None:
        self.rules.insert(0 if at_top else len(self.rules), rule); self.save()
    def remove_at(self, i: int) -> None:
        if 0 <= i < len(self.rules): del self.rules[i]; self.save()
    def move(self, i: int, delta: int) -> int:
        j = i + delta
        if 0 <= i < len(self.rules) and 0 <= j < len(self.rules):
            self.rules[i], self.rules[j] = self.rules[j], self.rules[i]
            self.save(); return j
        return i
    def needs_proc(self) -> bool:
        """True if any rule tests a process facet — lets a hot caller skip resolving the
        triggering process's provenance when no rule would use it."""
        return any(c.match.kind in ("exe", "cwd", "arg")
                   for r in self.rules for c in r.clauses)

    def decide(self, rel: str, box: str = "", proc=None) -> "str | None":
        target = PathTarget(rel, _subject_of(box, proc))
        for r in self.rules:
            if r.matches(target):
                return r.action
        return None

def load_file_rules() -> FileRules:
    return FileRules(file_rules_file())

def ut_split(data: bytes) -> list:
    """Split bytes into lines on \\n, keeping the terminator on each line (the last
    line keeps whatever it had). b"".join(result) == data exactly, so splicing
    these line-pieces and writing the bytes back is lossless — CR/CRLF, encoding,
    BOM and a missing final newline are all preserved."""
    parts = data.split(b"\n")
    lines = [p + b"\n" for p in parts[:-1]]
    if parts[-1]:
        lines.append(parts[-1])
    return lines

_NO_NL = b"\\ No newline at end of file\n"   # standard "no newline at EOF" marker

def _emit(out: list, prefix: bytes, line: bytes) -> None:
    # Each physical patch line ends in \n; a content line lacking its own newline
    # gets one (as terminator) plus the NO_NL marker, so parsing is unambiguous and
    # \r (CRLF) is preserved as content.
    if line.endswith(b"\n"):
        out.append(prefix + line)
    else:
        out.append(prefix + line + b"\n"); out.append(_NO_NL)

def make_patch(files: list) -> bytes:
    """files: list of dict(rel, lower, upper, created, deleted) with lower/upper as
    byte-line lists (from ut_split). Returns the unified-diff bytes."""
    out = []
    for f in files:
        a = b"/dev/null" if f["created"] else b"a/" + f["rel"].encode("utf-8","surrogateescape")
        b = b"/dev/null" if f["deleted"] else b"b/" + f["rel"].encode("utf-8","surrogateescape")
        out.append(b"--- " + a + b"\n"); out.append(b"+++ " + b + b"\n")
        ll, ul = f["lower"], f["upper"]
        for g in difflib.SequenceMatcher(None, ll, ul).get_grouped_opcodes(3):
            a1, a2, b1, b2 = g[0][1], g[-1][2], g[0][3], g[-1][4]
            out.append(f"@@ -{a1+1},{a2-a1} +{b1+1},{b2-b1} @@\n".encode())
            for tag, i1, i2, j1, j2 in g:
                if tag == "equal":
                    for ln in ll[i1:i2]: _emit(out, b" ", ln)
                else:
                    for ln in ll[i1:i2]: _emit(out, b"-", ln)
                    for ln in ul[j1:j2]: _emit(out, b"+", ln)
    return b"".join(out)

def parse_patch(data: bytes) -> dict:
    """Parse unified-diff bytes into an ordered {rel: filepatch} where filepatch is
    dict(rel, created, deleted, hunks=[dict(a1, b1, lines)]) and lines is the ordered
    [(op, byteline)] with op in ' '/'-'/'+'. Ordered lines let us re-serialize and
    edit hunks losslessly."""
    files = {}; cur = None; phys = ut_split(data); i = 0
    while i < len(phys):
        line = phys[i]; i += 1
        if line.startswith(b"--- "):
            aside = line[4:].rstrip(b"\n")
            bside = phys[i][4:].rstrip(b"\n") if i < len(phys) else b""; i += 1
            created = aside == b"/dev/null"; deleted = bside == b"/dev/null"
            relb = (aside if deleted else bside)[2:]   # strip "a/" or "b/"
            rel = relb.decode("utf-8","surrogateescape")
            cur = files[rel] = dict(rel=rel, created=created, deleted=deleted, hunks=[])
        elif line.startswith(b"@@") and cur is not None:
            m = re.search(rb"-(\d+)(?:,\d+)? \+(\d+)", line)
            if m is None:
                raise ValueError(f"parse_patch: malformed @@ header: {line!r}")
            cur["hunks"].append(dict(a1=int(m.group(1)) - 1,
                                     b1=int(m.group(2)) - 1, lines=[]))
        elif cur and cur["hunks"] and line[:1] in (b" ", b"-", b"+"):
            prefix = line[:1]; content = line[1:]
            if i < len(phys) and phys[i] == _NO_NL:
                i += 1; content = content[:-1]      # drop the terminator we added
            cur["hunks"][-1]["lines"].append((prefix.decode(), content))
    return files

def hunk_a_lines(h): return [bl for op, bl in h["lines"] if op != "+"]

def hunk_b_lines(h): return [bl for op, bl in h["lines"] if op != "-"]

def serialize_patch(files: dict) -> bytes:
    """Inverse of parse_patch: render an ordered {rel: filepatch} back to bytes."""
    out = []
    for fp in files.values():
        a = b"/dev/null" if fp["created"] else b"a/" + fp["rel"].encode("utf-8","surrogateescape")
        b = b"/dev/null" if fp["deleted"] else b"b/" + fp["rel"].encode("utf-8","surrogateescape")
        out.append(b"--- " + a + b"\n"); out.append(b"+++ " + b + b"\n")
        for h in fp["hunks"]:
            na, nb = len(hunk_a_lines(h)), len(hunk_b_lines(h))
            out.append(f"@@ -{h['a1']+1},{na} +{h['b1']+1},{nb} @@\n".encode())
            for op, line in h["lines"]:
                _emit(out, op.encode(), line)
    return b"".join(out)

def build_file_patch(rel, lower, upper, created, deleted) -> dict:
    """Parsed filepatch for one file from its lower/upper byte-line lists."""
    return parse_patch(make_patch([dict(rel=rel, lower=lower, upper=upper,
                                        created=created, deleted=deleted)]))[rel]

def sqlar_path(box_id) -> Path:
    # The box's single db, named by its stable integer id: <box_id>.sqlar.
    return state_home() / (str(box_id) + ".sqlar")

_SQLAR_SCHEMA_VERSION = 1  # marks the one-time DDL as applied (no migrations — unreleased)

class _DbHandle:
    __slots__ = ("conn", "lock", "pinned", "last_used", "closed")
    def __init__(self, conn):
        self.conn = conn
        self.lock = threading.RLock()   # serializes the one connection across threads
        self.pinned = False             # True while a live box's Index owns the handle
        self.last_used = time.monotonic()
        self.closed = False

_DB_REG: "dict[str, _DbHandle]" = {}    # db abspath -> cached handle (pinned if live)

_DB_REG_LOCK = threading.Lock()

_DB_IDLE_TTL = 30.0                      # keep a finished box's connection cached this

def _db_key(path) -> str:
    return os.path.abspath(os.fspath(path))

def _open_raw(path) -> "sqlite3.Connection":
    """Open + schema-init ONE raw connection (no WAL). check_same_thread=False: a
    registered handle is used by the serve thread and the UI thread, serialized by the
    handle's lock; a throwaway handle is used within a single helper call."""
    path = Path(path); path.parent.mkdir(parents=True, exist_ok=True)
    # 0700: the single db holds writer provenance (pid/exe/argv/env).
    old = os.umask(0o077)
    try:
        conn = sqlite3.connect(str(path), check_same_thread=False)
    finally:
        os.umask(old)
    try: os.chmod(str(path), 0o600)
    except OSError: pass
    conn.execute("PRAGMA busy_timeout=3000")
    # One connection per db ⇒ no concurrent writers ⇒ no WAL. Force rollback journal
    # (converting any file an older build left in WAL) and skip fsync for speed:
    # atomic-commit still survives a process crash; only power loss / OS crash can lose
    # recent commits — fine for this ephemeral, consolidated-on-exit index.
    conn.execute("PRAGMA journal_mode=DELETE")
    conn.execute("PRAGMA synchronous=OFF")
    # Gate the one-time DDL behind user_version so repeated opens (the common case)
    # skip straight to returning the connection.
    ver = conn.execute("PRAGMA user_version").fetchone()[0]
    if ver < _SQLAR_SCHEMA_VERSION:
        conn.executescript(
            "CREATE TABLE IF NOT EXISTS sqlar"
            "(name TEXT PRIMARY KEY, mode INT, mtime INT, sz INT, data BLOB,"
            " opaque INT DEFAULT 0, writer INT, last_writer INT);"
            "CREATE TABLE IF NOT EXISTS provenance"
            "(path TEXT PRIMARY KEY, pid INT, ppid INT, exe TEXT, argv TEXT);"
            "CREATE TABLE IF NOT EXISTS env"
            "(id INTEGER PRIMARY KEY AUTOINCREMENT, hash TEXT UNIQUE, env TEXT);"
            # process: one row per process INCARNATION, identified by (tgid,start) so a
            # reused pid is a distinct row; parent_id is the parent's row id (structure),
            # ppid is the parent's pid (display only).
            "CREATE TABLE IF NOT EXISTS process"
            "(id INTEGER PRIMARY KEY AUTOINCREMENT, tgid INT, start INT,"
            " ppid INT, parent_id INT, exe TEXT, cwd TEXT, argv TEXT, env_id INT,"
            " root INT DEFAULT 0, UNIQUE(tgid, start));"
            "CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT);"
            # outputs: one row per captured stdout/stderr write, attributed to the
            # writing process (process_id) via per-write ctx.pid. stream 0=stdout,
            # 1=stderr. content is the raw bytes of that one write.
            "CREATE TABLE IF NOT EXISTS outputs("
            " id INTEGER PRIMARY KEY AUTOINCREMENT, ts REAL, process_id INT,"
            " stream INT, content BLOB);")
        conn.execute(f"PRAGMA user_version = {_SQLAR_SCHEMA_VERSION}")
        conn.commit()
    return conn

class _DbConn:
    """A borrowed view of a db's ONE cached connection, returned by _sqlar_open(). It
    holds the db's lock until close(); close() does NOT close the connection — the
    handle stays cached (reused by the next borrow) and is reclaimed later by the idle
    sweep (after _DB_IDLE_TTL), at process exit, or when the box is deleted. A live
    box's handle is the same one its Index owns. Legacy
    `conn = _sqlar_open(p); try: ...; finally: conn.close()` call sites work unchanged.
    row_factory is applied per statement via a fresh cursor so it never leaks onto the
    shared connection (which the Index reads with default tuple rows)."""
    __slots__ = ("_h", "_rf")
    def __init__(self, h):
        self._h = h; self._rf = None
    @property
    def row_factory(self): return self._rf
    @row_factory.setter
    def row_factory(self, v): self._rf = v
    def execute(self, *a):
        cur = self._h.conn.cursor()
        if self._rf is not None: cur.row_factory = self._rf
        return cur.execute(*a)
    def executescript(self, *a): return self._h.conn.executescript(*a)
    def commit(self): return self._h.conn.commit()
    def rollback(self): return self._h.conn.rollback()
    def cursor(self): return self._h.conn.cursor()
    def close(self) -> None:
        try: self._h.conn.rollback()        # leave the cached/shared conn clean
        except sqlite3.Error: pass
        self._h.last_used = time.monotonic()
        self._h.lock.release()

def _db_sweep_idle() -> None:
    """Close + drop cached connections idle longer than _DB_IDLE_TTL. Never touches a
    pinned (live box) handle, nor one currently in use (lock held)."""
    now = time.monotonic()
    victims = []
    with _DB_REG_LOCK:
        for k, h in list(_DB_REG.items()):
            if h.pinned or now - h.last_used <= _DB_IDLE_TTL:
                continue
            if h.lock.acquire(blocking=False):   # skip if a borrow is active
                _DB_REG.pop(k, None); h.closed = True
                victims.append(h)
    for h in victims:
        try: h.conn.close()
        except sqlite3.Error: pass
        finally: h.lock.release()

def _sqlar_open(path) -> "_DbConn":
    """Borrow the ONE cached connection for `path` (ALWAYS close() it). Live box → its
    Index's registered handle; finished box → a cached handle kept _DB_IDLE_TTL after
    its last use. Held under the db lock so there is never a 2nd concurrent connection."""
    _db_sweep_idle()
    key = _db_key(path)
    while True:
        with _DB_REG_LOCK:
            h = _DB_REG.get(key)
            if h is None or h.closed:
                h = _DbHandle(_open_raw(path)); _DB_REG[key] = h
            h.last_used = time.monotonic()
        h.lock.acquire()
        with _DB_REG_LOCK:                       # re-check: not swept while we waited
            if _DB_REG.get(key) is h and not h.closed:
                h.last_used = time.monotonic()
                return _DbConn(h)
        h.lock.release()                         # swept under us → retry with a fresh one

def _sqlar_register(path) -> _DbHandle:
    """Adopt + register a live box's single connection (called once by its Index); pin
    it so the idle sweep never reclaims it while the box is running."""
    key = _db_key(path)
    with _DB_REG_LOCK:
        h = _DB_REG.get(key)
        if h is None or h.closed:
            h = _DbHandle(_open_raw(path)); _DB_REG[key] = h
        h.pinned = True
        h.last_used = time.monotonic()
        return h

def _sqlar_unpin(path) -> None:
    """Release a live box's pin so its cached connection becomes eligible for the idle
    sweep (kept _DB_IDLE_TTL for just-finished reads). The connection is NOT closed."""
    with _DB_REG_LOCK:
        h = _DB_REG.get(_db_key(path))
        if h is not None:
            h.pinned = False
            h.last_used = time.monotonic()

def _sqlar_unregister(path) -> None:
    """Drop + close a box's cached connection NOW (on box deletion, before unlinking the
    db). Helpers then reopen a fresh cached connection on demand."""
    key = _db_key(path)
    with _DB_REG_LOCK:
        h = _DB_REG.pop(key, None)
    if h is None: return
    with h.lock:
        try: h.conn.close()
        except sqlite3.Error: pass
        h.closed = True

def _sqlar_deflate(content: bytes):
    """Pack file/symlink content for a sqlar `data` blob, returning (data, sz) where
    `sz` is the true uncompressed length. Compression is DISABLED: content is stored
    verbatim, so sz == len(data) always — the sqlar "stored uncompressed" form. Read
    paths still transparently inflate any legacy rows where len(data) < sz, so existing
    compressed archives keep working; to re-enable compression, deflate here when it
    shrinks and return (comp, len(content))."""
    return (content, len(content))

def _sqlar_put_file(conn, name, content: bytes, mode, mtime_ns) -> None:
    """`mtime_ns` is integer nanoseconds (st_mtime_ns); stored as-is."""
    data, sz = _sqlar_deflate(content)
    conn.execute("INSERT INTO sqlar(name,mode,mtime,sz,data) VALUES(?,?,?,?,?)"
                 " ON CONFLICT(name) DO UPDATE SET mode=excluded.mode,"
                 " mtime=excluded.mtime, sz=excluded.sz, data=excluded.data",
                 (name, int(mode), int(mtime_ns), sz, sqlite3.Binary(data)))

def _sqlar_put_provenance(conn, name, prov: dict) -> None:
    conn.execute("INSERT OR REPLACE INTO provenance VALUES(?,?,?,?,?)",
                 (name, int(prov.get("pid") or 0), int(prov.get("ppid") or 0),
                  prov.get("exe") or "", json.dumps(prov.get("argv") or [])))

def _sqlar_put_symlink(conn, name, target: bytes, mtime_ns) -> None:
    """`mtime_ns` is integer nanoseconds (st_mtime_ns); stored as-is."""
    conn.execute("INSERT INTO sqlar(name,mode,mtime,sz,data) VALUES(?,?,?,?,?)"
                 " ON CONFLICT(name) DO UPDATE SET mode=excluded.mode,"
                 " mtime=excluded.mtime, sz=excluded.sz, data=excluded.data",
                 (name, stat_mod.S_IFLNK | 0o777, int(mtime_ns), len(target),
                  sqlite3.Binary(target)))

def _sqlar_put_tombstone(conn, name) -> None:
    conn.execute("INSERT INTO sqlar(name,mode,mtime,sz,data) VALUES(?,?,?,?,?)"
                 " ON CONFLICT(name) DO UPDATE SET mode=excluded.mode,"
                 " mtime=excluded.mtime, sz=excluded.sz, data=excluded.data",
                 (name, stat_mod.S_IFCHR, 0, 0, None))

def _sqlar_fetchone(path, sql, params=()):
    """One read-only SELECT against an archive on disk: fetchone() result, or None
    if the archive is missing or the query errors. Opens/closes its own handle —
    the shared scaffolding for all the standalone sqlar reader helpers below."""
    path = Path(path)
    if not path.exists(): return None
    conn = _sqlar_open(path)
    try: return conn.execute(sql, params).fetchone()
    except sqlite3.Error: return None
    finally: conn.close()

def _sqlar_fetchall(path, sql, params=()) -> list:
    """fetchall() counterpart of _sqlar_fetchone; [] when missing or errored."""
    path = Path(path)
    if not path.exists(): return []
    conn = _sqlar_open(path)
    try: return conn.execute(sql, params).fetchall()
    except sqlite3.Error: return []
    finally: conn.close()

def sqlar_list(path) -> list:
    """[(name, mode, mtime, sz)] sorted by name; [] if there is no archive.
    mtime is integer NANOSECONDS (st_mtime_ns)."""
    return _sqlar_fetchall(path, "SELECT name,mode,mtime,sz FROM sqlar ORDER BY name")

def sqlar_mode(path, name) -> "int | None":
    return sqlar_mode_mtime(path, name)[0]

def sqlar_mtime(path, name) -> "int | None":
    """Stored mtime as integer NANOSECONDS (st_mtime_ns), or None if missing."""
    return sqlar_mode_mtime(path, name)[1]

def sqlar_nonempty(path) -> bool:
    """True if the sqlar has at least one entry — cheaper than fetching all rows."""
    return _sqlar_fetchone(path, "SELECT 1 FROM sqlar LIMIT 1") is not None

def sqlar_mode_mtime(path, name) -> "tuple[int, int] | tuple[None, None]":
    """Fetch mode AND mtime in a single SELECT — use at call sites that need both."""
    row = _sqlar_fetchone(path, "SELECT mode, mtime FROM sqlar WHERE name=?", (name,))
    return (row[0], row[1]) if row else (None, None)

def box_id_of_sqlar(path) -> "int | None":
    """The box_id for a <box_id>.sqlar path: its filename stem (box_id IS the identity)."""
    try: return int(Path(path).stem)
    except (ValueError, TypeError): return None

def sqlar_content(path, name) -> "bytes | None":
    """Decompressed bytes for one entry (the symlink target for a symlink); None if
    missing or a tombstone. Reads EITHER rest form: an evicted/folded row (bytes
    compressed in `data`) or a permanently-resident row (data NULL; bytes in the pool
    blob at blob_path(box_id, rowid) — the uncompressed file on disk)."""
    row = _sqlar_fetchone(path, "SELECT rowid,sz,data FROM sqlar WHERE name=?",
                          (name,))
    if not row: return None
    rowid, sz, data = row
    if data is not None:
        data = bytes(data)
        return data if len(data) == sz else zlib.decompress(data)
    # Resident: bytes live in the pool blob (the permanent file rest form). Locate it
    # by the box's stable id (the sqlar filename stem) + the row's stable rowid.
    bid = box_id_of_sqlar(path)
    if bid is None: return None
    try:
        bp = blob_path(bid, rowid)
    except (ValueError, TypeError):
        return None
    try:
        return bp.read_bytes() if bp.exists() else None
    except OSError:
        return None

def sqlar_remove(path, name) -> int:
    """Drop one entry; unlink the archive when it empties of entries. Returns
    entries left."""
    path = Path(path)
    if not path.exists(): return 0
    conn = _sqlar_open(path)
    try:
        conn.execute("DELETE FROM sqlar WHERE name=?", (name,))
        conn.execute("DELETE FROM provenance WHERE path=?", (name,)); conn.commit()
        left = conn.execute("SELECT EXISTS(SELECT 1 FROM sqlar)").fetchone()[0]
    except sqlite3.Error as e:
        print(f"slopbox: sqlar_remove {path} {name}: {e}", file=sys.stderr)
        left = 1
    finally: conn.close()
    if left == 0:
        _sqlar_unregister(path)
        try: path.unlink()
        except OSError: pass
    return left

def _drop_sqlar_row_and_blob(path, box_id, rel) -> None:
    """Drop one sqlar row AND any permanent pool blob backing it. The row is removed
    FIRST: a failed removal must never leave a resident row whose blob is gone (a
    zombie reading as an empty file); a crash between the two only orphans the blob,
    which sweep_orphan_pools reclaims. `box_id` may be None (no blob check).
    Uses a single connection for both the rowid read and the delete."""
    path = Path(path)
    rel = rel.lstrip("/")
    if not path.exists(): return
    conn = _sqlar_open(path)
    bp = None
    try:
        try:
            row = conn.execute("SELECT rowid,data FROM sqlar WHERE name=?",
                               (rel,)).fetchone()
        except sqlite3.Error: row = None
        if box_id is not None and row is not None and row[1] is None:
            try: bp = blob_path(int(box_id), row[0])
            except (ValueError, TypeError): bp = None
        # Delete the row (and its provenance), then check emptiness — mirrors sqlar_remove
        try:
            conn.execute("DELETE FROM sqlar WHERE name=?", (name := rel,))
            conn.execute("DELETE FROM provenance WHERE path=?", (name,)); conn.commit()
            left = conn.execute("SELECT EXISTS(SELECT 1 FROM sqlar)").fetchone()[0]
        except sqlite3.Error: left = 1
    finally: conn.close()
    if left == 0:
        _sqlar_unregister(path)
        try: path.unlink()
        except OSError: pass
    if bp is not None:
        try:
            if bp.exists(): bp.unlink()
        except OSError: pass

_ACTIVE_CAP    = 4096

def outputs_list(path) -> list:
    # Omit `content` from the listing (it can be large); callers fetch it per-row.
    path = Path(path)
    if not path.exists(): return []
    conn = _sqlar_open(path); conn.row_factory = sqlite3.Row
    try:
        return [dict(r) for r in conn.execute(
            "SELECT id,ts,process_id,stream,length(content) AS len"
            " FROM outputs ORDER BY id").fetchall()]
    except sqlite3.Error:
        return []
    finally:
        conn.close()

def outputs_get(path, oid) -> "dict | None":
    path = Path(path)
    if not path.exists(): return None
    conn = _sqlar_open(path); conn.row_factory = sqlite3.Row
    try:
        r = conn.execute("SELECT * FROM outputs WHERE id=?", (oid,)).fetchone()
        return dict(r) if r else None
    except sqlite3.Error:
        return None
    finally:
        conn.close()

def process_list(path) -> list:
    """[(id, tgid, ppid, parent_id, exe, argv-list)] for the box's process table.
    `parent_id` is the parent's process-table ROW id (the structural link; NULL for a
    root/unknown parent); `ppid` is the parent's pid number, for display only."""
    rows = _sqlar_fetchall(path, "SELECT id,tgid,ppid,parent_id,exe,argv FROM process"
                                 " ORDER BY id")
    out = []
    for pid, tgid, ppid, parent_id, exe, argv in rows:
        try: av = json.loads(argv) if argv else []
        except (ValueError, TypeError): av = []
        out.append((pid, tgid, ppid, parent_id, exe or "", av))
    return out

def sqlar_meta_get(path, key: str) -> "str | None":
    """Read one small per-box meta string (e.g. the box's display `name`) from a
    sqlar on disk. None if absent."""
    row = _sqlar_fetchone(path, "SELECT value FROM meta WHERE key=?", (key,))
    return row[0] if row else None

def sqlar_meta_set(path, key: str, value: str) -> None:
    conn = _sqlar_open(path)
    try:
        conn.execute("INSERT INTO meta(key,value) VALUES(?,?)"
                     " ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                     (key, str(value)))
        conn.commit()
    finally: conn.close()

def _all_box_ids() -> set:
    """The set of integer box_ids that currently exist on disk: every <n>.sqlar stem
    plus every live/<n> backing dir. box_id IS the on-disk identity (file/dir stem),
    so this is a pure scan — no meta read needed."""
    out: set = set()
    sh = state_home()
    if sh.exists():
        for p in sh.glob("*.sqlar"):
            try: out.add(int(p.stem))
            except ValueError: pass
    lh = live_home()
    if lh.exists():
        for d in lh.iterdir():
            try: out.add(int(d.name))
            except ValueError: pass
    return out

def mint_box_id() -> int:
    """Mint a fresh, never-before-used box_id: max(existing on disk)+1.

    INVARIANT: exactly ONE process (the UI's Supervisor) ever calls this function.
    The scan-then-assign is therefore race-free without a cross-process lock.
    If that invariant were violated (e.g. two UI instances), collisions could occur.
    Cross-process locking is intentionally out of scope; the ChannelServer already
    prevents a second UI instance from starting (run_ui() checks ui_is_running()
    before launching). Called EAGERLY at box creation so a path is always derivable
    from the id."""
    return max(_all_box_ids(), default=0) + 1

def as_box_id(box_id) -> int:
    """box_id IS the identity now (the sqlar/backing stem), so this is the identity
    cast itself. Kept as a one-liner so the many call sites that pass a box's key and
    want its int read unchanged."""
    return int(box_id)

def sweep_orphan_pools() -> None:
    """Drop pool dirs whose box is gone: any blob/<box_id> with no surviving sqlar
    claiming that id (the box's sqlar was deleted/applied). Runs at UI startup beside
    the overlay-orphan sweep; cheap when the pool is empty."""
    root = pool_home()
    if not root.exists(): return
    live = set(_all_box_ids())
    for d in root.iterdir():
        if not d.is_dir(): continue
        try: bid = int(d.name)
        except ValueError: continue
        if bid not in live:
            rm_rf(d)

def process_roots(path) -> set:
    """The box's hierarchy-root ROW ids (process.root=1) read from a sqlar on disk —
    the finished-box counterpart to Index.roots(). Row ids (not tgids) so the proc tree
    links structurally, PID-reuse proof."""
    return {r[0] for r in _sqlar_fetchall(path, "SELECT id FROM process WHERE root=1")}

def sqlar_writer_id(path, rel) -> "int | None":
    """The process-table row id that LAST wrote `rel` (the sqlar `last_writer` tag),
    or None. Finished-box path: read straight from the consolidated single sqlar."""
    row = _sqlar_fetchone(path, "SELECT last_writer FROM sqlar WHERE name=?", (rel,))
    return row[0] if row and row[0] is not None else None

def sqlar_first_writer_id(path, rel) -> "int | None":
    """The process-table row id of the FIRST process to write `rel` (the sqlar `writer`
    tag), or None — the first-writer counterpart of sqlar_writer_id. Finished-box read."""
    row = _sqlar_fetchone(path, "SELECT writer FROM sqlar WHERE name=?", (rel,))
    return row[0] if row and row[0] is not None else None

def sqlar_proc_prov(path, proc_id) -> "dict | None":
    """Provenance (exe/cwd/argv) of ONE process row, read from a finished box's sqlar.
    None if the row id isn't recorded. Used to build the procs-pane filter Subject."""
    w = _sqlar_fetchone(path, "SELECT exe,cwd,argv FROM process WHERE id=?",
                        (proc_id,))
    if not w: return None
    try: argv = json.loads(w[2]) if w[2] else []
    except (ValueError, TypeError): argv = []
    return dict(exe=w[0] or "", cwd=w[1] or "", argv=argv)

def sqlar_first_writer_prov(path, rel) -> "dict | None":
    """Provenance (exe/cwd/argv) of the FIRST process to write `rel`, read from a
    stopped box's single sqlar (the `writer` tag → process row). Used to match a file
    rule's process facets at delete/finalize time. None if no writer is recorded."""
    w = _sqlar_fetchone(
        path,
        "SELECT process.tgid, process.ppid, process.exe, process.cwd, process.argv"
        " FROM sqlar JOIN process ON sqlar.writer=process.id"
        " WHERE sqlar.name=?", (rel,))
    if not w: return None
    try: argv = json.loads(w[4]) if w[4] else []
    except (ValueError, TypeError): argv = []
    return dict(pid=w[0], ppid=w[1], exe=w[2] or "", cwd=w[3] or "", argv=argv)

def process_env(path, proc_id) -> dict:
    """The deduped environment for one process row (resolved via env_id)."""
    row = _sqlar_fetchone(path,
                          "SELECT env.env FROM process JOIN env ON process.env_id=env.id"
                          " WHERE process.id=?", (proc_id,))
    if not row or not row[0]: return {}
    try: return json.loads(row[0])
    except (ValueError, TypeError): return {}

def root_cmd(sid: str) -> list:
    """The box's command: argv of its FIRST root process row (process.root=1, lowest
    id) in <sid>.sqlar. [] if no such row exists. The persisted single source of truth,
    surviving after the runner exits; a re-run box has several roots — this is the
    original launch's command."""
    row = _sqlar_fetchone(sqlar_path(sid), "SELECT argv FROM process WHERE root=1"
                                           " ORDER BY id LIMIT 1")
    if not row or not row[0]: return []
    try: return list(json.loads(row[0]))
    except (ValueError, TypeError): return []

_PROV_ENV_KEYS = ("USER", "PWD", "SSH_CONNECTION", "TERM", "_")

def _proc_status_field(pid: int, field: str) -> "int | None":
    """One integer field (e.g. Tgid, PPid) from /proc/<pid>/status. None if gone."""
    try:
        with open(f"/proc/{pid}/status", "rb") as f:
            for ln in f:
                if ln.startswith(field.encode() + b":"):
                    return int(ln.split()[1])
    except (OSError, ValueError, IndexError):
        pass
    return None

def tgid_of(pid: int) -> int:
    """Resolve a FUSE ctx.pid (a thread id) to its thread-group id via
    /proc/<pid>/status Tgid; falls back to pid itself if unreadable."""
    t = _proc_status_field(pid, "Tgid")
    return t if t is not None else int(pid)

def _parse_proc_stat(pid: int) -> "tuple[int, int]":
    """Parse /proc/<pid>/stat and return (ppid, start_time).  Both fields are 0 on
    any error.  The comm field may contain spaces and parentheses, so the split is
    anchored after the last ')' (rfind) — the same dance used by both callers."""
    try:
        with open(f"/proc/{pid}/stat", "rb") as f:
            st = f.read()
        rp = st.rfind(b")")
        rest = st[rp + 2:].split() if rp >= 0 else []
        ppid = int(rest[1]) if len(rest) >= 2 else 0
        start_time = int(rest[19]) if len(rest) >= 20 else 0
        return ppid, start_time
    except (OSError, ValueError):
        return 0, 0

def _proc_start_time(pid: int) -> int:
    """The process's start time (field 22 of /proc/<pid>/stat, jiffies since boot).
    0 if unreadable. Cheap and stable for a live process; used to detect PID reuse so
    a provenance cache entry is not reused across two different processes (LOW-1)."""
    return _parse_proc_stat(pid)[1]

def read_provenance(pid: int, full_env: bool = False) -> dict:
    """Best-effort writer provenance from host /proc/<pid>. Read SYNCHRONOUSLY from
    the write handler while the caller is still blocked (and thus still in /proc).
    Every field is best-effort — a vanished pid yields zeros/empties, never raises
    (/proc reads are the one place best-effort OSError is acceptable). full_env
    captures the whole environment (for the process table when -e env capture is
    on); otherwise only the small _PROV_ENV_KEYS subset is kept."""
    info = dict(pid=int(pid), ppid=0, exe="", cwd="", argv=[], env={}, start_time=0)
    base = f"/proc/{pid}"
    try:
        with open(f"{base}/cmdline", "rb") as f:
            raw = f.read()
        info["argv"] = [p.decode("utf-8", "surrogateescape")
                        for p in raw.split(b"\x00") if p]
    except OSError: pass
    try: info["exe"] = os.readlink(f"{base}/exe")
    except OSError: pass
    try: info["cwd"] = os.readlink(f"{base}/cwd")
    except OSError: pass
    # Parse /proc/<pid>/stat via the shared helper (comm may contain spaces/parens).
    ppid, start_time = _parse_proc_stat(pid)
    info["ppid"] = ppid; info["start_time"] = start_time
    try:
        with open(f"{base}/environ", "rb") as f:
            env_raw = f.read()
        env = {}
        for kv in env_raw.split(b"\x00"):
            if b"=" not in kv: continue
            k, _, v = kv.partition(b"=")
            ks = k.decode("utf-8", "surrogateescape")
            if full_env or ks in _PROV_ENV_KEYS:
                env[ks] = v.decode("utf-8", "surrogateescape")
        info["env"] = env
    except OSError: pass
    return info

def _kind_of_mode(mode: int) -> str:
    if stat_mod.S_ISCHR(mode): return "whiteout"
    if stat_mod.S_ISDIR(mode): return "dir"
    if stat_mod.S_ISLNK(mode): return "symlink"
    return "file"

class Index:
    """The single per-instance database (sqlar_path(sid)) + a write-through RAM
    mirror. There is exactly ONE sqlite file per box; this class is the set of
    operations on it that the FUSE layer needs — there is no separate index.db.

    On every mutating FUSE op the entry row is upserted into the `sqlar` table
    right then — per file, as it happens, with no separate finished form to build
    later. A regular file's bytes go to its pool blob (row resident, data = NULL) or,
    for a small file, are deflated inline into the row at release; either way the row
    is a complete rest form the moment the file is closed. The hot read paths
    (lookup/getattr/readdir) touch ONLY the RAM mirror; the db is written on
    mutation and read for provenance / process detail. Opened
    check_same_thread=False — the serve loop owns all mutations (under
    self._lock); the UI thread reads concurrently and relies on the lock for
    SQLite safety and GIL-protected dict reads for the RAM mirror.

    A box always records each unique tgid into the `process` table here; with -e
    (env capture) on it also dedups the writer's full environment via the `env`
    table. The process row id tags modified file entries (the `writer` column)
    and captured stdout/stderr outputs."""

    def __init__(self, backing: Path):
        self.backing = Path(backing)
        # The single db: state_home/<box_id>.sqlar. The box_id is the backing dir name.
        self.db_path = sqlar_path(self.backing.name)
        # Register this box's ONE connection (created + schema-init here) so every
        # standalone helper that touches this same db borrows it under the shared lock
        # instead of opening a rival handle — there is never a 2nd live connection, so
        # no WAL. _db is the raw connection; _lock (below) is the db's shared lock.
        self._dbh = _sqlar_register(self.db_path)
        self._db = self._dbh.conn
        # Process identity is per-incarnation (tgid,start) — 16-bit PIDs roll over
        # during a big build, so a tgid alone cannot identify a process. _proc_cache
        # maps (tgid,start)->row id (the exact incarnation); _proc_current maps
        # tgid->(start,row id) for the LATEST-seen incarnation, which is what new
        # references (writer/parent_id) resolve to.
        self._proc_cache: dict = {}       # (tgid,start) -> process.id
        self._proc_current: dict = {}     # tgid -> (start, process.id) latest seen
        self._prov_cache: dict = {}       # host pid -> provenance dict (hot-path cache)
        self._prov_by_proc: dict = {}     # process.id -> provenance dict
        # Live "active set": tgid -> last-seen monotonic, stamped every time the FUSE
        # serve thread or the echo/channel path records/sees a tgid (in-memory only, NOT
        # persisted — the persistent `process` table keeps the full history). Reads
        # come from the UI thread; the dict is GIL-atomic like the other RAM mirrors.
        self._active: dict = {}           # tgid -> last-seen monotonic
        self._dead_since: dict = {}       # tgid -> first monotonic kill(0) saw it dead
        self._env_capture = False         # -e: capture each writer's full environment
        # The box's hierarchy ROOTS: the process-table ROW IDS of every `slopbox -- cmd`
        # launch (one per run; a re-run adds another). Newly-inserted process rows bubble
        # up their PPid chain until they reach a root, so the table stays one connected
        # FOREST. Row ids (not tgids) so PID reuse cannot conflate two launches.
        # Authoritative source is the `process.root` column (loaded here, kept in sync
        # by process_from_prov).
        self._roots: set = set()
        self._whiteouts: set = set()
        self._kinds: dict = {}
        self._opaque: set = set()
        self._targets: dict = {}          # rel -> symlink target bytes (RAM mirror)
        # Dir-listing cache validity (see _scan_dir_cached): per-PARENT-dir
        # generation counters bumped by every mirror mutation under that dir,
        # plus a global epoch for subtree ops (prune_subtree/reparent) that
        # touch many dirs at once. Per-dir scope matters: git-status-style
        # workloads write .git/* every run — that must not invalidate the
        # cached listings of unrelated directories.
        self._dir_gens: dict = {}
        self.mirror_epoch = 0
        self._lock = self._dbh.lock       # the db's shared lock (SQLite + mirror)
        self._load_mirror()
        # Stable pool id == the box's identity == the backing dir / sqlar stem.
        self.box_id = int(self.backing.name)

    def set_env_capture(self, on: bool) -> None:
        self._env_capture = bool(on)

    def _load_mirror(self) -> None:
        for path, mode, opaque, sz, data in self._db.execute(
                "SELECT name, mode, opaque, sz, data FROM sqlar"):
            kind = _kind_of_mode(mode)
            if kind == "whiteout":
                self._whiteouts.add(path)
            else:
                self._kinds[path] = kind
            if kind == "symlink" and data is not None:
                blob = bytes(data)
                self._targets[path] = (blob if len(blob) == sz
                                       else zlib.decompress(blob))
            if opaque:
                self._opaque.add(path)
        # Repopulate the incarnation caches + hierarchy roots from any rows recorded
        # by earlier runs of this box (rerun reopens the same db). _proc_current keeps
        # the highest-id incarnation per tgid as "current" (ORDER BY id ASC overwrites).
        for rid, tgid, start, ppid, parent_id, exe, cwd, argv, root in self._db.execute(
                "SELECT id,tgid,start,ppid,parent_id,exe,cwd,argv,root FROM process"
                " ORDER BY id"):
            try: av = json.loads(argv) if argv else []
            except (ValueError, TypeError): av = []
            self._proc_cache[(tgid, start or 0)] = rid
            self._proc_current[tgid] = (start or 0, rid)
            self._prov_by_proc[rid] = dict(pid=tgid, ppid=ppid or 0,
                                           parent_id=parent_id, exe=exe or "",
                                           cwd=cwd or "", argv=av)
            if root:
                self._roots.add(rid)

    @staticmethod
    def _row_mode(kind, mode) -> int:
        if kind == "whiteout": return stat_mod.S_IFCHR
        return int(mode)

    # ── fast RAM accessors (hot path) ───────────────────────────────────────
    def _bump_dir(self, path: str, also_self: bool = False) -> None:
        """Invalidate the cached listing of `path`'s parent dir (the listing the
        mutation changes). also_self: the mutation altered `path`'s OWN listing
        too (its opaque flag). Called under _lock by the mirror mutators."""
        p = path.rsplit("/", 1)[0] if "/" in path else ""
        self._dir_gens[p] = self._dir_gens.get(p, 0) + 1
        if also_self:
            self._dir_gens[path] = self._dir_gens.get(path, 0) + 1

    def dirlist_gen(self, rel: str) -> tuple:
        """The dir-listing validity stamp for dir `rel`: (global epoch, this
        dir's mutation count). GIL-atomic read — see _scan_dir_cached."""
        return (self.mirror_epoch, self._dir_gens.get(rel, 0))

    def has_overlay_state(self) -> bool:
        return bool(self._whiteouts or self._kinds or self._opaque)

    def kind_of(self, rel: str) -> "str | None":
        if rel in self._whiteouts: return "whiteout"
        return self._kinds.get(rel)

    def symlink_target(self, rel: str) -> "bytes | None":
        """The symlink target bytes for `rel`, or None if rel is not a symlink in the
        mirror. Served straight from RAM (no disk readlink)."""
        if self._kinds.get(rel) != "symlink": return None
        return self._targets.get(rel)

    def is_opaque(self, rel: str) -> bool:
        return rel in self._opaque

    def hidden_by_opaque(self, rel: str) -> bool:
        if not self._opaque: return False
        parts = rel.split("/") if rel else []
        for i in range(len(parts)):
            anc = "/".join(parts[:i])
            if anc in self._opaque: return True
        return False

    def all_kinds(self) -> tuple:
        """Snapshot of every path with a (non-whiteout) index kind. UI-thread read of
        the RAM mirror; GIL-atomic like the other accessors."""
        return tuple(self._kinds.keys())

    def children_of(self, rel: str) -> tuple:
        """Index entries that are DIRECT children of dir `rel`, for the merged
        readdir scan: (whiteouts, present). `whiteouts` is the set of child names
        that whiteout a lower entry (remove from the listing); `present` is the
        set of child names that have an index kind (ensure present in the
        listing). Names only — caller resolves each via kind_of/_resolve_st."""
        prefix = (rel + "/") if rel else ""
        whiteouts, present = set(), set()
        for wp in self._whiteouts:
            if wp.startswith(prefix) and "/" not in wp[len(prefix):]:
                whiteouts.add(wp[len(prefix):])
        for kp in self._kinds:
            if kp.startswith(prefix) and kp != rel and "/" not in kp[len(prefix):]:
                present.add(kp[len(prefix):])
        return whiteouts, present

    # ── provenance / process table ──────────────────────────────────────────
    def _prov_key(self, pid: int) -> tuple:
        """Cache key that survives PID reuse: (pid, /proc start_time)."""
        return (pid, _proc_start_time(pid))

    def _touch_active(self, tgid: int) -> None:
        """Re-stamp a tgid in the active set (last-seen monotonic) and keep the set
        bounded: when it crosses the cap, drop the oldest-seen dead entries so RAM
        stays bounded even if the proc pane is never opened to drive retirement.
        Serve/echo-thread write; GIL-atomic dict ops (Index threading contract)."""
        now = time.monotonic()
        self._active[tgid] = now
        if len(self._active) > _ACTIVE_CAP:
            overflow = len(self._active) - _ACTIVE_CAP
            for t, _seen in _heapq.nsmallest(
                    overflow + 16, self._active.items(), key=lambda kv: kv[1]):
                if len(self._active) <= _ACTIVE_CAP: break
                if t == tgid or _pid_alive(t): continue
                self._active.pop(t, None); self._dead_since.pop(t, None)

    def writer_for(self, pid: int) -> int:
        """Resolve the FUSE ctx.pid (a TID) to its tgid and return that tgid's CURRENT
        incarnation's process-table row id, inserting it (and its deduped env) on first
        sight. Identity is (tgid,start) so PID reuse makes a new row, not a dedup into a
        stale one. Always returns an id even if /proc is gone. The id tags the file entry."""
        tgid = tgid_of(pid)
        self._touch_active(tgid)                 # active-set: re-stamp on every sight
        # With env capture on, grab the FULL environment for the process table; the
        # subset-cached provenance_of() is enough otherwise. Resolve the parent to
        # its tgid and hand off to the single insert/dedup path (which also bubbles
        # the PPid chain up to the box root so the table stays one connected tree).
        prov = read_provenance(pid, full_env=True) if self._env_capture \
            else self.provenance_of(pid)
        start = int(prov.get("start_time") or 0)
        cached = self._proc_cache.get((tgid, start))
        if cached is not None: return cached
        ppid_tgid = tgid_of(prov["ppid"]) if prov.get("ppid") else 0
        return self.process_from_prov(dict(tgid=tgid, start=start, ppid=ppid_tgid,
                                           parent_pid=prov.get("ppid") or 0,
                                           exe=prov["exe"], cwd=prov.get("cwd") or "",
                                           argv=prov["argv"],
                                           env=prov.get("env") or {})) or tgid

    def process_from_prov(self, prov: dict, root: bool = False) -> "int | None":
        """Insert/dedup a process row from a provenance dict (tgid/start/ppid/exe/argv/
        env, all HOST-namespace, consistent with the FUSE path) and return its row id.
        Returns None if no
        tgid was supplied. Identity is (tgid,start): a reused pid with a new start_time is
        a NEW row, never a dedup into the prior incarnation. The parent is recorded FIRST
        (so parent_id is the parent's CURRENT incarnation ROW id, not a pid); the PPid
        chain bubbles up to the box root so the table stays one connected forest. `root`
        marks this incarnation a hierarchy root (a `slopbox -- cmd` launch) — the bubbling
        boundary; its ROW id is remembered in the RAM `_roots` mirror.
        `prov["start"]` defaults to /proc start_time of `tgid`; `prov["parent_pid"]` (the
        parent's actual host pid, for resolving the parent's own incarnation) defaults to
        `prov["ppid"]`."""
        tgid = int(prov.get("tgid") or 0)
        if not tgid: return None
        start = int(prov.get("start_time") or prov.get("start") or 0) \
            or _proc_start_time(tgid)
        self._touch_active(tgid)                  # active-set: re-stamp on every sight
        cached = self._proc_cache.get((tgid, start))
        if cached is not None:
            self._proc_current[tgid] = (start, cached)
            if root:                              # promote an already-recorded row
                self._roots.add(cached)
                with self._lock:
                    self._db.execute("UPDATE process SET root=1 WHERE id=?", (cached,))
                    self._db.commit()
            return cached
        ppid_tgid = int(prov.get("ppid") or 0)
        exe = prov.get("exe") or ""
        cwd = prov.get("cwd") or ""
        argv = prov.get("argv") or []
        env = prov.get("env") or {}
        # Resolve the parent to its CURRENT incarnation ROW id (NULL for a root or an
        # unreachable parent) by recording the parent FIRST — this is what makes the
        # table one connected forest and what the tree links by. A root is its own
        # boundary: never walk above a launch into the runner's host ancestry.
        parent_id = None
        if not root:
            parent_id = self._resolve_parent(
                int(prov.get("parent_pid") or ppid_tgid or 0),
                _depth=int(prov.get("_bubble_depth") or 0),
                _seen=prov.get("_bubble_seen") or None)
        with self._lock:
            eid = None
            if self._env_capture:
                env_json = json.dumps(env, sort_keys=True)
                env_hash = hashlib.sha256(env_json.encode()).hexdigest()
                self._db.execute("INSERT OR IGNORE INTO env(hash,env) VALUES(?,?)",
                                 (env_hash, env_json))
                eid = self._db.execute("SELECT id FROM env WHERE hash=?",
                                       (env_hash,)).fetchone()[0]
            cur = self._db.execute(
                "INSERT OR IGNORE INTO process"
                "(tgid,start,ppid,parent_id,exe,cwd,argv,env_id,root)"
                " VALUES(?,?,?,?,?,?,?,?,?)",
                (tgid, start, ppid_tgid, parent_id, exe, cwd, json.dumps(argv), eid,
                 1 if root else 0))
            _ = cur.rowcount   # (race-safe: re-read the row id below regardless)
            pid_row = self._db.execute(
                "SELECT id FROM process WHERE tgid=? AND start=?",
                (tgid, start)).fetchone()[0]
            self._db.commit()
        self._proc_cache[(tgid, start)] = pid_row
        self._proc_current[tgid] = (start, pid_row)
        if root:
            self._roots.add(pid_row)
        self._prov_by_proc[pid_row] = dict(pid=tgid, ppid=ppid_tgid,
                                           parent_id=parent_id, exe=exe,
                                           cwd=cwd, argv=argv)
        return pid_row

    def _resolve_parent(self, ppid: int, _depth: int = 0,
                        _seen: "set | None" = None) -> "int | None":
        """Record the parent process `ppid` (and so its whole PPid chain) and return its
        CURRENT incarnation ROW id, so the per-box process table forms ONE forest rooted
        at each `slopbox -- cmd` launch and a child's parent_id is a row id (PID-reuse
        proof), never a tgid. Best-effort: a failed ancestor /proc read links a minimal
        row and stops, never raises. Returns None when there is no recordable parent.
        STOPS at: ppid<=1 (init — no host system procs), a depth/cycle cap (64 levels /
        seen-set, matching _lower_has / display_path). A parent that is itself a root is
        recorded normally; its own _resolve_parent is skipped (root is the boundary)."""
        if ppid <= 1: return None                # reached init; no system procs
        ptgid = tgid_of(ppid) or ppid
        if _depth >= 64: return self._current_row(ptgid)  # depth cap
        if _seen is None: _seen = set()
        if ptgid in _seen: return self._current_row(ptgid)  # cycle guard
        _seen.add(ptgid)
        # Key the parent on its LIVE (tgid,start): if its pid was reused since we last
        # saw it, that's a new incarnation row — never reuse the stale _proc_current.
        pstart = _proc_start_time(ppid)
        cached = self._proc_cache.get((ptgid, pstart)) if pstart else None
        if cached is not None:                   # this exact incarnation already linked
            self._proc_current[ptgid] = (pstart, cached)
            return cached
        try:
            pprov = read_provenance(ppid, full_env=self._env_capture)
        except OSError:
            pprov = dict(ppid=0, exe="", argv=[], env={}, start_time=pstart)
        # A live parent yields its own ppid (walk continues up); a vanished parent
        # yields ppid 0 — a minimal row is still linked, then the next frame stops at
        # ppid<=1 since the grandparent is unreachable.
        parent = dict(tgid=ptgid, start_time=pprov.get("start_time") or pstart,
                      ppid=tgid_of(pprov.get("ppid")) if pprov.get("ppid") else 0,
                      parent_pid=pprov.get("ppid") or 0,
                      exe=pprov.get("exe") or "", argv=pprov.get("argv") or [],
                      env=pprov.get("env") or {},
                      _bubble_depth=_depth + 1, _bubble_seen=_seen)
        return self.process_from_prov(parent)    # recurses up; returns parent row id

    def _current_row(self, tgid: int) -> "int | None":
        """The process-table ROW id of `tgid`'s latest-seen incarnation, or None."""
        cur = self._proc_current.get(tgid)
        return cur[1] if cur is not None else None

    def proc_info(self, row_id: int) -> "tuple | None":
        """(tgid, ppid, parent_id, exe, argv-list) for ANY recorded process ROW id, in or
        out of the active set — the tree builder uses it to resolve connector ancestors by
        row id (PID-reuse proof). RAM-only read of the GIL-atomic _prov_by_proc cache
        (Index threading contract). None if the row id was never recorded."""
        prov = self._prov_by_proc.get(row_id)
        if prov is None: return None
        return (prov["pid"], prov["ppid"], prov.get("parent_id"),
                prov["exe"] or "", list(prov["argv"]))

    def roots(self) -> set:
        """The box's hierarchy-root ROW ids (the PPid-bubbling / tree-walk boundary).
        GIL-atomic read of the RAM mirror (Index threading contract)."""
        return set(self._roots)

    def live_processes(self) -> list:
        """htop-style live view of the box's ACTIVE SET: only the tgids that are
        RUNNING right now, each resolved to its CURRENT incarnation row and decorated
        from the in-memory caches (NO full-table scan, cost O(active set)). Returns
        [(id, tgid, ppid, parent_id, exe, argv-list, alive, dead)] with alive=True /
        dead=False for every row: a tgid that kill(0) reports gone is dropped from the
        active set immediately (no "recently finished" grace), the same display rule the
        live changes/outputs panes use. The active set is tgid-keyed for liveness probing,
        but display/tree linkage is by row id (PID-reuse proof). The persistent `process`
        table keeps the full history. UI-thread read of the GIL-atomic dicts (contract)."""
        out = []
        for tgid in list(self._active):
            if not _pid_alive(tgid):          # exited: drop from the live pane at once
                self._active.pop(tgid, None)
                self._dead_since.pop(tgid, None)
                continue
            pid_row = self._current_row(tgid)
            prov = self._prov_by_proc.get(pid_row) if pid_row is not None else None
            if prov is not None:
                ppid, parent_id, exe, argv = (prov["ppid"], prov.get("parent_id"),
                                              prov["exe"], prov["argv"])
            else:
                ppid, parent_id, exe, argv = 0, None, "", []
            out.append((pid_row if pid_row is not None else tgid, tgid, ppid,
                        parent_id, exe or "", list(argv), True, False))
        out.sort(key=lambda r: r[1])
        return out

    def add_output(self, process_id: "int | None", stream: int, content: bytes,
                   ts: "float | None" = None) -> None:
        """Append one captured stdout/stderr write to the box's single sqlar via THIS
        handle (the one writer, under self._lock). `process_id` is the
        writer's process-table row id (already resolved via writer_for(ctx.pid) in the
        sink write handler); stream 0=stdout, 1=stderr; content is that write's bytes."""
        with self._lock:
            self._db.execute(
                "INSERT INTO outputs(ts,process_id,stream,content) VALUES(?,?,?,?)",
                (ts if ts is not None else time.time(), process_id, int(stream),
                 sqlite3.Binary(content)))
            self._db.commit()

    def provenance_of(self, pid: int) -> dict:
        """Provenance for a pid, cached per (pid, start_time) for this process."""
        key = self._prov_key(pid)
        cached = self._prov_cache.get(key)
        if cached is not None: return cached
        prov = read_provenance(pid)
        self._prov_cache[key] = prov
        return prov

    # ── mutations (write-through; upsert into the single sqlar) ──────────────
    def _upsert(self, path, mode, writer_id, opaque) -> None:
        """Upsert one entry row with data=NULL (filled at consolidate). opaque None
        means leave as-is on update; writer_id None means leave as-is on update.
        `writer` records the FIRST process to touch the path (set once); `last_writer`
        records the MOST RECENT (advanced on every named write)."""
        # opaque=None → NULL here so the ON CONFLICT clause can distinguish
        # "leave as-is" (NULL → COALESCE keeps existing) from "set to 0" (0).
        # COALESCE(?,0) in the INSERT VALUES defaults a fresh row to 0 when opaque
        # is None (NULL), matching the original behaviour.
        op = int(opaque) if opaque is not None else None
        self._db.execute(
            "INSERT INTO sqlar(name,mode,mtime,sz,data,opaque,writer,last_writer)"
            " VALUES(?,?,0,0,NULL,COALESCE(?,0),?,?)"
            " ON CONFLICT(name) DO UPDATE SET"
            "  mode=excluded.mode,"
            "  opaque=COALESCE(excluded.opaque, sqlar.opaque),"
            "  writer=COALESCE(sqlar.writer, excluded.writer),"
            "  last_writer=COALESCE(excluded.last_writer, sqlar.last_writer)",
            (path, int(mode), op, writer_id, writer_id))

    def set_entry(self, path, kind, mode, writer_id, op,
                  detail: str = "", opaque=None, target=None,
                  mtime_ns=None) -> None:
        with self._lock:
            self._bump_dir(path, also_self=opaque is not None)
            self._upsert(path, self._row_mode(kind, mode), writer_id, opaque)
            if kind == "symlink" and target is not None:
                # Put the target into the row IMMEDIATELY (deflated, sz=len) so the
                # row is the single source of truth — no fold-at-consolidate desync.
                tgt = target if isinstance(target, (bytes, bytearray)) else \
                    str(target).encode("utf-8", "surrogateescape")
                data, sz = _sqlar_deflate(bytes(tgt))
                m = int(mtime_ns) if mtime_ns is not None else 0
                self._db.execute(
                    "UPDATE sqlar SET sz=?, data=?, mtime=? WHERE name=?",
                    (sz, sqlite3.Binary(data), m, path))
            elif mtime_ns is not None:
                # Dir (or any non-symlink) row: record mtime so the synthesized stat
                # served from the mirror carries a stable timestamp.
                self._db.execute("UPDATE sqlar SET mtime=? WHERE name=?",
                                 (int(mtime_ns), path))
            self._db.commit()
            # RAM mirror — mutate under the SAME lock.
            if kind == "whiteout":
                self._whiteouts.add(path); self._kinds.pop(path, None)
                self._targets.pop(path, None)
            else:
                self._kinds[path] = kind; self._whiteouts.discard(path)
                if kind == "symlink":
                    if target is not None:
                        self._targets[path] = bytes(tgt)
                else:
                    self._targets.pop(path, None)
            if opaque is not None:
                if opaque: self._opaque.add(path)
                else: self._opaque.discard(path)

    def del_entry(self, path, op: str = "del", writer_id=None) -> None:
        with self._lock:
            self._bump_dir(path)
            self._db.execute("DELETE FROM sqlar WHERE name=?", (path,))
            self._db.commit()
            self._whiteouts.discard(path); self._kinds.pop(path, None)
            self._opaque.discard(path); self._targets.pop(path, None)

    def row_id(self, rel: str) -> "int | None":
        """The sqlar rowid for `rel` — names the blob in the pool."""
        with self._lock:
            row = self._db.execute(
                "SELECT rowid FROM sqlar WHERE name=?", (rel,)).fetchone()
        return row[0] if row else None

    def rename_row(self, old: str, new: str, writer_id=None) -> None:
        """Single-row in-place rename: preserves rowid (and thus blob address).
        Deletes any pre-existing row at `new`, then renames `old`->>`new` in-place.
        Updates the RAM mirror accordingly."""
        with self._lock:
            self._bump_dir(old); self._bump_dir(new)
            # drop any pre-existing entry at the destination
            self._db.execute("DELETE FROM sqlar WHERE name=?", (new,))
            # rename in-place (rowid preserved)
            if writer_id is not None:
                self._db.execute(
                    "UPDATE sqlar SET name=?, last_writer=? WHERE name=?",
                    (new, writer_id, old))
            else:
                self._db.execute(
                    "UPDATE sqlar SET name=? WHERE name=?", (new, old))
            self._db.commit()
            # mirror: move old -> new, dropping any prior new
            for s in (self._whiteouts, self._opaque):
                had = new in s; s.discard(new)
                if old in s: s.discard(old); s.add(new)
                elif had: pass  # old not present; new was, now gone — correct
            kind = self._kinds.pop(new, None)  # drop any pre-existing new kind
            old_kind = self._kinds.pop(old, None)
            if old_kind is not None:
                self._kinds[new] = old_kind
            # move the symlink target alongside the kind (drop any pre-existing new)
            self._targets.pop(new, None)
            old_tgt = self._targets.pop(old, None)
            if old_tgt is not None:
                self._targets[new] = old_tgt

    def prune_subtree(self, prefix: str) -> None:
        pref = prefix + "/"
        pref_hi = pref[:-1] + chr(ord(pref[-1]) + 1)  # prefix range upper bound
        with self._lock:
            self.mirror_epoch += 1
            # Free the pool blobs of any file rows under the prefix before dropping the
            # rows — otherwise the bytes leak (the row that named them is gone).
            for rid, mode in self._db.execute(
                    "SELECT rowid, mode FROM sqlar WHERE name >= ? AND name < ?",
                    (pref, pref_hi)).fetchall():
                if stat_mod.S_ISREG(mode):
                    try: blob_path(self.box_id, rid).unlink()
                    except OSError: pass
            self._db.execute("DELETE FROM sqlar WHERE name >= ? AND name < ?",
                             (pref, pref_hi))
            self._db.commit()
            for s in (self._whiteouts, self._opaque):
                for p in [p for p in s if p.startswith(pref)]: s.discard(p)
            for p in [p for p in self._kinds if p.startswith(pref)]:
                self._kinds.pop(p, None)
            for p in [p for p in self._targets if p.startswith(pref)]:
                self._targets.pop(p, None)

    def reparent(self, old: str, new: str, writer_id) -> None:
        """Rename the whole subtree old/ -> new/ IN PLACE (UPDATE name, keep rowids).
        Rowid stability is required so file blobs (addressed by box_id+rowid) never
        move on disk across a directory rename."""
        oldp = old + "/"; newp = new + "/"
        oldp_hi = oldp[:-1] + chr(ord(oldp[-1]) + 1)  # prefix range upper bound
        with self._lock:
            self.mirror_epoch += 1
            rows = self._db.execute(
                "SELECT rowid, name, mode, opaque FROM sqlar"
                " WHERE name >= ? AND name < ?",
                (oldp, oldp_hi)).fetchall()
            for rowid, path, mode, opaque in rows:
                np = newp + path[len(oldp):]
                # drop any pre-existing row at the destination (keep source rowid)
                self._db.execute("DELETE FROM sqlar WHERE name=? AND rowid!=?",
                                 (np, rowid))
                self._db.execute("UPDATE sqlar SET name=? WHERE rowid=?", (np, rowid))
            self._db.commit()
            for _rowid, path, mode, opaque in rows:
                np = newp + path[len(oldp):]
                kind = _kind_of_mode(mode)
                self._whiteouts.discard(path); self._kinds.pop(path, None)
                self._opaque.discard(path)
                # symlink target moves with the row (rowid + data preserved on disk);
                # carry the RAM mirror over too.
                old_tgt = self._targets.pop(path, None)
                self._targets.pop(np, None)
                # also drop any pre-existing mirror entry at the new name
                self._whiteouts.discard(np); self._kinds.pop(np, None)
                self._opaque.discard(np)
                if kind == "symlink" and old_tgt is not None:
                    self._targets[np] = old_tgt
                if kind == "whiteout": self._whiteouts.add(np)
                else: self._kinds[np] = kind
                if opaque: self._opaque.add(np)

    # ── consolidate accessors ────────────────────────────────────
    def deletions(self) -> list:
        with self._lock:
            snap = list(self._whiteouts)
        return sorted(snap)

    def opaque_dirs(self) -> list:
        with self._lock:
            snap = list(self._opaque)
        return sorted(snap)

    def writer_id_for(self, path: str) -> "int | None":
        """The process-table row id that LAST wrote `path` (the sqlar `last_writer`
        tag), or None. Keys the proc pane → used to focus the writer when jumping to
        procs. UI-thread read of the single sqlar handle: take _lock so it never races
        the serve thread's writes on the shared connection (Index threading contract)."""
        return self._writer_id_col(path, "last_writer")

    def first_writer_id_for(self, path: str) -> "int | None":
        """The process-table row id that FIRST wrote `path` (the sqlar `writer` tag), or
        None — the first-writer counterpart of writer_id_for, used to pin the procs pane
        to a change's writers. UI-thread read under _lock (Index threading contract)."""
        return self._writer_id_col(path, "writer")

    def _writer_id_col(self, path: str, col: str) -> "int | None":
        try:
            with self._lock:
                row = self._db.execute(
                    f"SELECT {col} FROM sqlar WHERE name=?", (path,)).fetchone()
        except sqlite3.Error: return None
        return row[0] if row and row[0] is not None else None

    def proc_prov_for(self, row_id: int) -> "dict | None":
        """Provenance (exe/cwd/argv) of ANY recorded process ROW id, from the GIL-atomic
        RAM mirror — for the procs-pane filter Subject. None if the row was never seen."""
        prov = self._prov_by_proc.get(row_id)
        if prov is None: return None
        return dict(exe=prov["exe"] or "", cwd=prov.get("cwd") or "",
                    argv=list(prov["argv"]))

    def writer_provenance(self, path: str) -> "dict | None":
        # No lock: consolidate-only precondition — called post-teardown with the box
        # dead and the serve thread stopped, so there is no concurrent writer.
        return self._prov_for_writer(path, "last_writer")

    def first_writer_provenance(self, path: str) -> "dict | None":
        """Provenance (exe/cwd/argv) of the FIRST process to write `path` — the writer a
        file rule's process match is locked to (see _passthrough). Takes _lock: callable
        from the serve thread on the live db. None if the path has no recorded writer."""
        with self._lock:
            return self._prov_for_writer(path, "writer")

    def _prov_for_writer(self, path: str, col: str) -> "dict | None":
        cur = self._db.execute(
            f"SELECT {col} FROM sqlar WHERE name=?", (path,)).fetchone()
        if not cur or cur[0] is None: return None
        cached = self._prov_by_proc.get(cur[0])
        if cached is not None: return dict(cached)
        w = self._db.execute(
            "SELECT tgid,ppid,exe,cwd,argv FROM process WHERE id=?", (cur[0],)).fetchone()
        if not w: return None
        try: argv = json.loads(w[4]) if w[4] else []
        except (ValueError, TypeError): argv = []
        return dict(pid=w[0], ppid=w[1], exe=w[2], cwd=w[3] or "", argv=argv)

    def row_mtime(self, rel: str) -> int:
        """The row's stored mtime (integer ns), or 0 if missing. Used on the hot read
        path to synthesize dir/symlink stat without an on-disk lstat."""
        with self._lock:
            row = self._db.execute(
                "SELECT mtime FROM sqlar WHERE name=?", (rel,)).fetchone()
        return int(row[0]) if row and row[0] is not None else 0

    def row_mode_mtime(self, rel: str):
        """(mode, mtime_ns) for the row, or (None, 0). Hot read path: synthesize a
        dir/symlink stat from the row without touching disk."""
        with self._lock:
            row = self._db.execute(
                "SELECT mode, mtime FROM sqlar WHERE name=?", (rel,)).fetchone()
        if not row: return (None, 0)
        return (int(row[0]), int(row[1]) if row[1] is not None else 0)

    def file_row(self, rel: str):
        """Return (rowid, mode, sz, mtime, data) for a file row, or None.
        Used on the hot read path to serve evicted (data NOT NULL) files."""
        with self._lock:
            row = self._db.execute(
                "SELECT rowid, mode, sz, mtime, data FROM sqlar WHERE name=?",
                (rel,)).fetchone()
        return row  # None or (rowid, mode, sz, mtime, data)

    def mark_resident(self, rel: str) -> None:
        """After a fault-in: NULL the row's data+sz to restore the live invariant
        (blob in pool, row empty). Called only after the blob is safely written."""
        with self._lock:
            self._db.execute(
                "UPDATE sqlar SET data=NULL, sz=0 WHERE name=?", (rel,))
            self._db.commit()

    def set_blob(self, rel: str, data: bytes, sz: int, mode: int,
                 mtime_ns: int, writer_id) -> None:
        """Write an evicted-row (data NOT NULL) for `rel` — called from the FUSE
        layer's write-buffer release path to deflate a buffered file straight into
        the sqlar row, bypassing a pool blob.  Reuses _upsert so the RAM mirror
        (_kinds) and row shape stay consistent with what set_entry/consolidate expect.
        `data` is already deflated (raw-or-zlib per _sqlar_deflate); `sz` is the
        uncompressed length."""
        with self._lock:
            self._bump_dir(rel)
            # _upsert ensures the row exists + updates mode/writer mirror.
            self._upsert(rel, mode, writer_id, None)
            self._db.execute(
                "UPDATE sqlar SET data=?, sz=?, mtime=?, mode=? WHERE name=?",
                (sqlite3.Binary(data), sz, int(mtime_ns), int(mode), rel))
            self._db.commit()
            # RAM mirror: ensure kind is "file" (whiteout cleared by _upsert logic).
            self._kinds[rel] = "file"
            self._whiteouts.discard(rel); self._targets.pop(rel, None)

    def close(self) -> None:
        # Unpin the box's single connection but KEEP it cached so just-finished reads
        # reuse it; the idle sweep (after _DB_IDLE_TTL), box deletion, or process exit
        # reclaims it.
        try: _sqlar_unpin(self.db_path)
        except (sqlite3.Error, AttributeError): pass

def _is_overlay_marker(name: str) -> bool:
    """Guard against legacy char-device whiteout / opaque-directory marker names
    (.wh..opq, .wh..wh.*) appearing in the upper. These are never created by this
    overlay model — kept as a defensive filter in upper_has_changes and cleanup_dirs."""
    return name == ".wh..opq" or name.startswith(".wh..wh.")

def upper_has_changes(up: Path) -> bool:
    """True if the overlay upper holds a STRAY on-disk artifact (a leftover file).
    Under the no-mirror invariant up/ holds no change artifacts at all — files live
    in the content pool, dirs/symlinks live in the sqlar rows — so this normally
    returns False and the box's reviewable state is gauged from its sqlar
    (sqlar_list) by every caller. Kept as a defensive stray-artifact
    detector (and to recognize a pre-invariant orphan tree)."""
    for root, _dirs, files in os.walk(up, onerror=lambda _e: None):
        if any(not _is_overlay_marker(nm) for nm in files):
            return True
    return False

def is_whiteout(p: Path) -> bool:
    try:
        st = p.stat(); return stat_mod.S_ISCHR(st.st_mode) and st.st_rdev == 0
    except OSError: return False

def _is_symlink(p: Path) -> bool:
    """True if p is itself a symlink (lstat, never follows). Used to refuse writing
    THROUGH a symlink onto the host / into the upper from the UI thread (LOW-3)."""
    return Path(p).is_symlink()

def _restore_host_mtime_ns(host, mtime_ns, is_symlink: bool) -> None:
    """Set host's mtime to exactly `mtime_ns` integer nanoseconds (atime too). For a
    symlink, utime the link itself (follow_symlinks=False). Best-effort/no-op if
    mtime_ns is None or the platform can't utime a symlink."""
    if mtime_ns is None: return
    n = int(mtime_ns)
    try:
        os.utime(host, ns=(n, n), follow_symlinks=not is_symlink)
    except (OSError, NotImplementedError):
        pass

def remove_upper_entry(p: Path) -> None:
    """Remove one upper artifact: a file, a directory tree, or a char-device
    whiteout. No-op if nothing is there (e.g. an opacity-only deletion)."""
    try:
        if p.is_dir() and not p.is_symlink() and not is_whiteout(p):
            shutil.rmtree(p)
        elif p.exists() or p.is_symlink() or is_whiteout(p):
            p.unlink()
    except FileNotFoundError:
        pass

def cleanup_dirs(upper: Path, start: Path) -> None:
    """Walk up from `start` toward `upper`, removing directories that hold no real
    change. A dir with only opaque markers is removed only when the lower has
    nothing there (a freshly-created dir); otherwise the markers record deletions
    and the dir is kept. Dirs with files/whiteouts stop the walk."""
    d = start
    while d != upper and upper in d.parents:
        try: children = list(d.iterdir())
        except OSError: break
        if any(not _is_overlay_marker(c.name) for c in children):
            break
        if children:
            rel = d.relative_to(upper)
            if Path("/").joinpath(*rel.parts).is_dir():
                break
        parent = d.parent
        try: shutil.rmtree(d)
        except OSError: break
        d = parent

def _open_index_ro(shm_dir) -> "Index | None":
    """Open the session's single db for consolidate. None if absent."""
    sp = sqlar_path(Path(shm_dir).name)
    if not sp.exists(): return None
    try: return Index(Path(shm_dir))
    except sqlite3.Error: return None

def consolidate(shm_dir, session_id: str, index: "Index | None" = None,
                progress=None) -> None:
    """Settle a box's overlay into its single sqlar at rest — PLACEMENT ONLY, no file
    rules. Symlinks/dirs already carry their truth in the row (target/mode/mtime written
    at creation — no fold walk); each regular file picks a per-file rest form by size
    (small → folded sqlar blob; large → kept as a PERMANENT pool file, the row left
    resident with data NULL); deletions/opacity become tombstone rows. Writer
    provenance per path is recorded. Afterwards the sqlar (plus any kept pool files) is
    the box's complete at-rest form and the backing tree can be freed. Idempotent.

    Apply/discard of file rules is NOT done here — those run on box DELETE or on an
    explicit UI action (ChangeReview.apply/discard), never on box stop. So consolidate
    runs at stop purely to make the box reviewable; nothing reaches the host.

    `progress`, if given, is called as progress(done, total) after each upper entry is
    folded so a UI can render a fraction while consolidate runs off the loop."""
    upper = Path(shm_dir) / "up"
    if not upper.is_dir(): return
    own_index = index is None
    idx = index if index is not None else _open_index_ro(shm_dir)
    sp = sqlar_path(session_id)
    # Pool id for this box: needed to locate blob files.
    box_id = as_box_id(session_id)
    sq = _sqlar_open(sp)
    def _set_blob(rel, content: bytes, mode, mtime_ns) -> None:
        data, sz = _sqlar_deflate(content)
        sq.execute("UPDATE sqlar SET mode=?, mtime=?, sz=?, data=? WHERE name=?",
                   (int(mode), int(mtime_ns), sz, sqlite3.Binary(data), rel))
        if sq.execute("SELECT changes()").fetchone()[0] == 0:
            # No pre-existing row (orphaned overlay with no live index): create it.
            _sqlar_put_file(sq, rel, content, mode, mtime_ns)
    def _prov(rel):
        return idx.writer_provenance(rel) if idx is not None else None
    moved = []
    blob_paths_to_evict = []
    try:
        # 1) symlinks/dirs need NO fold walk: their rows already carry the truth — the
        #    symlink target was written into the row at creation (set_entry target=...),
        #    dir rows hold mode/mtime, and neither has an on-disk artifact. (A stray file
        #    artifact under up/ is unexpected but still folded gracefully below.) The walk
        #    here only catches such strays; the symlink-fold path is gone.
        paths = []
        for root, dirs, names in os.walk(upper, onerror=lambda _e: None):
            for nm in names: paths.append(Path(root) / nm)
        ordered = sorted(set(paths)); total_upper = len(ordered)
        for done, ap in enumerate(ordered, 1):
            rel = str(ap.relative_to(upper))
            if progress is not None:
                try: progress(done, total_upper)
                except Exception: pass
            try:
                if ap.is_file() and not ap.is_symlink():
                    # Regular files no longer live in up/; a file here is unexpected
                    # but handle it gracefully (fold it in, then remove).
                    st = ap.stat()
                    _set_blob(rel, ap.read_bytes(), st.st_mode, st.st_mtime_ns)
                    p = _prov(rel)
                    if p: _sqlar_put_provenance(sq, rel, p)
                else:
                    continue                                 # dirs handled by cleanup
            except OSError:
                continue
            moved.append(ap)
        # 1b) regular-file rows: bytes live in pool blobs (data IS NULL). Place by size:
        #     a large file stays a PERMANENT pool file (row resident: data NULL, blob kept
        #     on disk — record its real size/mode/mtime so offline readers report correct
        #     metadata); a small file folds into the sqlar blob and its pool file is evicted.
        file_rows = sq.execute(
            "SELECT rowid, name, mode FROM sqlar WHERE data IS NULL").fetchall()
        for rowid, rel, mode in file_rows:
            if not stat_mod.S_ISREG(mode): continue         # dir/symlink/tombstone rows
            bp = blob_path(box_id, rowid)
            if not bp.exists(): continue                     # no blob (not yet written)
            try:
                bst = os.stat(bp)
                mtime_ns = bst.st_mtime_ns
                size = bst.st_size                            # authoritative — never read the bytes for sizing
            except OSError:
                continue
            if size >= POOL_RESIDENT_MIN:
                # Large file: stays a PERMANENT pool blob — it is ALREADY at rest. Only
                # the row's metadata needs settling, so read nothing (st_size suffices);
                # reading multi-MB blobs here only to measure + discard them was the bulk
                # O(total-bytes) pass that froze the UI.
                sq.execute("UPDATE sqlar SET mode=?, mtime=?, sz=?, data=NULL WHERE name=?",
                           (int(mode), int(mtime_ns), size, rel))
                if sq.execute("SELECT changes()").fetchone()[0] == 0:
                    # Orphaned overlay with no live-index row: create a resident row.
                    sq.execute("INSERT INTO sqlar(name,mode,mtime,sz,data) VALUES(?,?,?,?,NULL)",
                               (rel, int(mode), int(mtime_ns), size))
                # blob NOT evicted: it IS the rest form.
            else:
                # Small file: fold the bytes inline and evict the pool blob (read only here,
                # where the bytes are actually needed for the inline blob).
                try: content = bp.read_bytes()
                except OSError: continue
                _set_blob(rel, content, mode, mtime_ns)
                blob_paths_to_evict.append(bp)
            p = _prov(rel)
            if p: _sqlar_put_provenance(sq, rel, p)
        # 2) deletions (whiteouts) from the index -> tombstone rows (already present, but
        #    re-assert for the orphaned-overlay path) + provenance.
        if idx is not None:
            for rel in idx.deletions():
                host = Path("/") / rel
                if host.exists() or host.is_symlink():
                    _sqlar_put_tombstone(sq, rel)
                    p = _prov(rel)
                    if p: _sqlar_put_provenance(sq, rel, p)
                else:
                    sq.execute("DELETE FROM sqlar WHERE name=?", (rel,))
                    sq.execute("DELETE FROM provenance WHERE path=?", (rel,))
            # 3) opaque dirs: expand into per-lower-child tombstones for children not
            #    re-materialized in the upper or the pool.
            for drel in idx.opaque_dirs():
                lower = Path("/") / drel
                if not lower.is_dir(): continue
                for lroot, ldirs, lfiles in os.walk(lower, onerror=lambda _e: None):
                    for nm in ldirs + lfiles:
                        crel = os.path.relpath(os.path.join(lroot, nm), "/")
                        # Re-materialized children live in the ROW now (dirs/symlinks have
                        # no upper artifact; files keep a pool blob or an evicted row). A
                        # non-tombstone row of ANY kind means the child was re-created.
                        row = sq.execute("SELECT rowid, mode, data FROM sqlar WHERE name=?",
                                         (crel,)).fetchone()
                        if row and not stat_mod.S_ISCHR(row[1]):
                            if stat_mod.S_ISREG(row[1]):
                                if blob_path(box_id, row[0]).exists() or row[2] is not None:
                                    continue                  # file present in pool or row
                                # regular row with neither blob nor data: fall through to
                                # tombstone (the bytes are gone).
                            else:
                                continue                      # dir/symlink re-created
                        _sqlar_put_tombstone(sq, crel)
        sq.commit()
    finally:
        sq.close()
    for ap in moved:
        remove_upper_entry(ap); cleanup_dirs(upper, ap.parent)
    for bp in blob_paths_to_evict:
        try: bp.unlink()
        except OSError: pass
    if own_index and idx is not None:
        idx.close()

def unconsolidate(shm_dir) -> None:
    """Ensure the empty `up/` discovery sentinel exists inside `shm_dir`. The Index
    mirror (dirs, symlinks) and pool blobs (regular files) are the authoritative
    state — nothing is rebuilt on disk. Idempotent."""
    upper = Path(shm_dir) / "up"
    upper.mkdir(parents=True, exist_ok=True)

def _pid_alive(pid: int) -> bool:
    if not pid: return False
    try:
        os.kill(pid, 0); return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True

def _pidfd_alive(pidfd: int) -> bool:
    """True while the process the pidfd refers to is still running. A pidfd becomes
    readable once its process exits (Linux >=5.3), so 'not readable' == alive. Wrap-
    immune: the pidfd names one exact process incarnation, never a reused pid."""
    if pidfd is None or pidfd < 0:
        return False
    try:
        r, _, _ = select.select([pidfd], [], [], 0)
    except (OSError, ValueError):
        return False
    return not r

def _box_running(s) -> bool:
    """True iff box `s` has a live runner root right now — the wrap-immune liveness
    that Session.status uses. PID-reuse-safe: never trusts s.pid (a reused pid could
    make a finished box look alive and be read or freed as if it were running)."""
    return bool(s and s.live and s.run_pidfd >= 0 and _pidfd_alive(s.run_pidfd))

def rm_rf(path) -> None:
    """Remove a tree, fixing perms (backing dirs are created with restrictive modes,
    e.g. 0700, so a plain rmtree would fail on subdirectories)."""
    path = str(path)
    if not os.path.exists(path) and not os.path.islink(path):
        return
    def _fix_and_retry(func, p, _exc):
        try:
            os.chmod(p, 0o700)
            par = os.path.dirname(p)
            if par: os.chmod(par, 0o700)
        except OSError:
            pass
        try: func(p)
        except OSError: pass
    try:
        shutil.rmtree(path, onexc=_fix_and_retry)
    except TypeError:
        shutil.rmtree(path, onerror=_fix_and_retry)
    if os.path.exists(path):
        for root, dirs, files in os.walk(path, topdown=False):
            for nm in files + dirs:
                fp = os.path.join(root, nm)
                try: os.chmod(fp, 0o700)
                except OSError: pass
                try: (os.rmdir if os.path.isdir(fp) and not os.path.islink(fp)
                      else os.unlink)(fp)
                except OSError: pass
        try: os.chmod(path, 0o700); os.rmdir(path)
        except OSError: pass

@dataclass
class Session:
    session_id: str                # the box's internal key, str(box_id) — its identity
    cmd: list
    shm_dir: str = ""              # the session backing dir: live/<box_id> (up only)
    # status is DERIVED (see .status): a box is "running" iff it currently has a live
    # runner root (kernel-derived liveness via the held pidfd). These flags only record
    # WHY a stopped box stopped — they never make a box appear running.
    killed: bool = False           # the box was killed (vs exiting on its own)
    errored: bool = False          # bwrap never launched (nothing was written)
    exit_code: "int | None" = None
    live: bool = False
    has_sqlar: bool = False                           # the single db has entries
    box_id: int = 0              # the box's stable integer id (== int(session_id))
    name: str = ""              # the box's mutable user-facing NAME label
    run_pid: int = 0              # the live runner's host pid (set at register)
    run_pidfd: int = -1           # held pidfd of the live runner; liveness handle, closed at teardown
    parent_box_id: "int | None" = None  # parent's box_id (kernel-derived); None for top-level
    # started time comes from the box's on-disk ctime (the id is numeric and carries no
    # timestamp); pid is the live runner's host pid.
    @property
    def started(self) -> float:
        return box_ctime(self.box_id or self.session_id)
    @property
    def pid(self) -> int:
        return self.run_pid
    @property
    def upper(self) -> str: return str(Path(self.shm_dir) / "up")
    @property
    def status(self) -> str:
        # Box status is a property of its running roots, NOT a stored flag flipped on
        # stop: a box is "running" iff it has a live runner root right now (kernel-
        # derived liveness via the held pidfd — wrap-immune). With no running root the
        # box is stopped, and the flags say why. A box discovered from disk (no held
        # pidfd this UI run) is, by definition, not running.
        if _box_running(self):
            return "running"
        if self.killed:
            return "killed"
        if self.errored:
            return "error"
        return "finished"
    def to_dict(self) -> dict:
        d = asdict(self)
        d["upper"] = self.upper
        d["status"] = self.status
        d["started"] = self.started; d["pid"] = self.pid
        # session_id stays == str(box_id) (the internal key); `name` is the mutable label.
        # The derived dotted display `path` is injected at the Supervisor _emit boundary.
        return d

def _parent_box_id_of(af) -> "int | None":
    v = sqlar_meta_get(af, "parent_box_id")
    try: return int(v) if v else None
    except (ValueError, TypeError): return None

def discover_sessions() -> dict:
    """Find sessions purely from the on-disk box_id identity: live/<box_id> backing
    dirs (an un-consolidated/orphaned overlay) plus the single <box_id>.sqlar under
    state_home. The key is str(box_id). The NAME label and parent_box_id pointer come
    from the sqlar meta; the command is the root process row in the sqlar."""
    out = {}
    lh = live_home()
    if lh.exists():
        for d in lh.iterdir():
            if not (d / "up").is_dir(): continue          # not a slopbox backing dir
            sid = d.name
            if not BOX_ID_RE.match(sid): continue            # not a box_id-named dir
            # No held pidfd for a disk-discovered box (its runner, if any, belonged to a
            # prior UI run we don't own), so .status derives to "finished" — correct: a
            # box this UI isn't running can't be "running".
            out[sid] = Session(
                session_id=sid, box_id=int(sid), cmd=root_cmd(sid), shm_dir=str(d),
                live=False)
    sd = state_home()
    if sd.exists():
        for af in sd.glob("*.sqlar"):
            sid = af.name[:-len(".sqlar")]
            if not BOX_ID_RE.match(sid): continue            # not a box_id-named sqlar
            s = out.get(sid)
            if s is None:
                s = out[sid] = Session(
                    session_id=sid, box_id=int(sid), cmd=root_cmd(sid),
                    shm_dir=str(live_dir(sid)), live=False)
            s.has_sqlar = sqlar_nonempty(af)
            # The NAME label and parent pointer come from meta. Empty/missing parent meta
            # means a top-level box (parent None). Without the parent pointer a nested box
            # would come back top-level and apply/promote would target the host.
            s.name = sqlar_meta_get(af, "name") or ""
            s.parent_box_id = _parent_box_id_of(af)
    return out

def _write_host_hunk(dst: Path, new_lower: list) -> dict:
    """Write new_lower bytes to dst on the host, refusing to write through a symlink.
    Returns dict(ok=True) on success, dict(ok=False, error=...) on failure."""
    if _is_symlink(dst):
        return dict(ok=False, error="refusing to write through a symlink")
    try:
        dst.parent.mkdir(parents=True, exist_ok=True)
        dst.write_bytes(b"".join(new_lower))
    except OSError as e:
        return dict(ok=False, error=str(e))
    return dict(ok=True)

def _build_hunks_display(ll: list, ul: list, groups: list) -> list:
    """Build the hunk display list from lower/upper byte-line lists and diff groups."""
    def disp(b): return b.decode("utf-8", "replace").rstrip("\r\n")
    hunks = []
    for gi, group in enumerate(groups):
        a1, a2 = group[0][1], group[-1][2]
        b1, b2 = group[0][3], group[-1][4]
        lines = [("hdr", f"@@ -{a1+1},{a2-a1} +{b1+1},{b2-b1} @@")]
        for tag, i1, i2, j1, j2 in group:
            if tag == "equal":
                for ln in ll[i1:i2]: lines.append((" ", disp(ln)))
            else:
                for ln in ll[i1:i2]: lines.append(("-", disp(ln)))
                for ln in ul[j1:j2]: lines.append(("+", disp(ln)))
        hunks.append(dict(index=gi, lines=lines))
    return hunks

_STRUCT_MAX = 4 * 1024 * 1024   # skip structural diff above this; parsers are heavy

def _pem_marker(data: bytes):
    head = data[:8192]
    if b"-----BEGIN " not in head: return None
    if b"BEGIN CERTIFICATE REQUEST" in head:
        return ["openssl", "req", "-in", "{in}", "-noout", "-text"]
    if b"BEGIN X509 CRL" in head:
        return ["openssl", "crl", "-in", "{in}", "-noout", "-text"]
    if b"BEGIN PKCS7" in head:
        return ["openssl", "pkcs7", "-in", "{in}", "-print_certs", "-text", "-noout"]
    if b"BEGIN CERTIFICATE" in head or b"BEGIN TRUSTED CERTIFICATE" in head:
        return ["openssl", "x509", "-in", "{in}", "-noout", "-text"]
    if b"PRIVATE KEY" in head or b"BEGIN EC PARAMETERS" in head:
        return ["openssl", "pkey", "-in", "{in}", "-noout", "-text"]
    if b"-----BEGIN " in head:   # other PEM blob — best-effort structure dump
        return ["openssl", "asn1parse", "-in", "{in}"]
    return None

def differ_for(mtype: str, data: bytes):
    """Return (argv_template, label) for a recognized binary, else None.
    `mtype` is `magic.from_buffer(data)`; `data` the leading bytes (for sniffing)."""
    if not data: return None
    mt = (mtype or "").lower()
    if "elf" in mt:
        return (["readelf", "-Wa", "{in}"], "ELF (readelf -Wa)")
    pem = _pem_marker(data)
    if pem is not None:
        return (pem, "PEM (openssl)")
    if data[:1] == b"\x30":   # DER SEQUENCE — ASN.1 (cert/key/etc.)
        if "certificate" in mt:
            return (["openssl", "x509", "-inform", "DER", "-in", "{in}",
                     "-noout", "-text"], "DER cert (openssl x509)")
        return (["openssl", "asn1parse", "-inform", "DER", "-in", "{in}"],
                "DER ASN.1 (openssl asn1parse)")
    if "current ar archive" in mt or mt.startswith("ar archive") or "ar archive" in mt:
        return (["ar", "t", "{in}"], "ar archive (ar t)")
    if "zip archive" in mt or data[:2] == b"PK":
        return (["unzip", "-l", "{in}"], "zip (unzip -l)")
    if "tar archive" in mt or "gzip compressed" in mt or "bzip2" in mt or "xz compressed" in mt:
        return (["tar", "-tvf", "{in}"], "tar (tar -tvf)")
    return None

class ChangeSource(abc.ABC):
    """The single abstraction barrier over the two concrete representations of a
    box's change set — {the live in-RAM Index (canonical)} vs {the box's one
    sqlar archive (at rest)}. Every change-review operation talks only to this
    interface; `ChangeReview._source(sid)` is the SOLE place that branches
    live-vs-consolidated. Implementors: LiveUpper, SqlarArchive."""

    @abc.abstractmethod
    def current_bytes(self, rel) -> "bytes | None":
        """Bytes for a changed entry in the current state, or None for whiteouts,
        directories, or missing entries."""

    @abc.abstractmethod
    def current_mode(self, rel) -> "int | None":
        """st_mode for the changed entry, or None on any error / missing entry."""

    @abc.abstractmethod
    def current_mtime(self, rel) -> "int | None":
        """Stored-side mtime as integer NANOSECONDS (st_mtime_ns) for the changed entry
        (the diff-base for the stale check), or None if unavailable."""

    @abc.abstractmethod
    def entries(self) -> list:
        """Cheap change records [{path,kind,size}, ...] — NO per-entry host I/O.
        kind is the host-I/O-free class (deleted/symlink/changed); is_text/stale/
        precise created-vs-modified are decorated lazily per visible row by the UI."""

    @abc.abstractmethod
    def remove(self, rel) -> None:
        """Drop/forget a change after a whole-file apply or discard."""

    @abc.abstractmethod
    def discard(self, rel) -> None:
        """Drop a change without writing the host (handles whiteouts and deletions)."""

    @abc.abstractmethod
    def settle(self, rel) -> None:
        """Post-hunk-op cleanup: forget the change if it no longer differs."""

    @abc.abstractmethod
    def write_current(self, rel, data: bytes) -> "dict | None":
        """Revert bytes back into the current state (for discard_hunk). Returns a
        dict(ok=False, error=...) to abort, or None on success."""

    @abc.abstractmethod
    def apply_plan(self, rel) -> "dict | None":
        """How to materialise this change onto the host: dict(kind=...) where kind
        is 'delete' | 'symlink' (target=) | 'dir' | 'file' (data=, mode=). None if
        the entry is not in this source."""

class LiveUpper(ChangeSource):
    """Change substrate for a running box: the live in-RAM Index (canonical; files in
    the pool, dirs/symlinks as rows, deletions and opaque-dir expansions tracked
    in memory)."""
    _OPAQUE_DELETE_CAP = 500

    def __init__(self, review, sid):
        self.review, self.sid = review, sid
        self.s = review.reg.sessions.get(sid)
        self.idx = review.reg.indexes.get(sid)    # only the live in-RAM index (no reopen)

    def current_bytes(self, rel):
        # Files: bytes live in the pool blob (or an evicted row) — read via the same
        # row-or-pool path the offline reader uses. Dirs/symlinks/whiteouts: no bytes.
        rel = rel.lstrip("/")
        if self.idx is None: return None
        if self.idx.kind_of(rel) != "file": return None
        return sqlar_content(sqlar_path(self.sid), rel)

    def current_mode(self, rel):
        rel = rel.lstrip("/")
        if self.idx is None: return None
        mode, _mt = self.idx.row_mode_mtime(rel)
        return mode

    def current_mtime(self, rel):
        rel = rel.lstrip("/")
        if self.idx is None: return None
        kind = self.idx.kind_of(rel)
        if kind == "file":
            # File mtime is the blob/row mtime (consolidate stamps it); fall back to
            # the row's stored mtime for an evicted/unflushed file.
            row = self.idx.file_row(rel)
            if row is not None and row[3]:
                return row[3]
        _mode, mt = self.idx.row_mode_mtime(rel)
        return mt or None

    def entries(self):
        # Enumerate straight from the live Index mirror — files (pool/row), dirs and
        # symlinks (rows), whiteouts and opaque-dir expansions. No os.walk of up/:
        # dirs/symlinks have no on-disk artifact, files live in the pool, not up/.
        if not self.s or self.idx is None: return []
        idx = self.idx
        entries: dict = {}
        for rel in sorted(idx.all_kinds()):
            kind = idx.kind_of(rel)
            if kind == "dir":
                continue                       # bare dirs aren't change rows
            if kind == "symlink":
                tgt = idx.symlink_target(rel) or b""
                entries[rel] = dict(path=rel, kind="symlink", size=len(tgt))
            elif kind == "file":
                row = idx.file_row(rel)   # (rowid, mode, sz, mtime, data)
                size = 0
                if row is not None:
                    rid, _m, sz, _mt, data = row
                    if data is not None:
                        size = sz             # evicted: row carries the size
                    else:
                        # resident: bytes in the pool blob; row sz may be 0 pre-fold.
                        try: size = blob_path(idx.box_id, rid).stat().st_size
                        except OSError: size = sz
                entries[rel] = dict(path=rel, kind="changed", size=size)
        if idx is not None:
            for rel in idx.deletions():
                entries[rel] = dict(path=rel, kind="deleted", size=0)
            added = 0
            for drel in idx.opaque_dirs():
                if added >= self._OPAQUE_DELETE_CAP: break
                lower = Path("/") / drel
                if not lower.is_dir(): continue
                for lroot, ldirs, lfiles in os.walk(lower, onerror=lambda _e: None):
                    for nm in ldirs + lfiles:
                        rel = os.path.relpath(os.path.join(lroot, nm), "/")
                        if rel in entries: continue
                        entries[rel] = dict(path=rel, kind="deleted",
                                            size=0, is_text=False)
                        added += 1
                        if added >= self._OPAQUE_DELETE_CAP: break
                    if added >= self._OPAQUE_DELETE_CAP: break
        return [entries[k] for k in sorted(entries)]

    def _drop_index_row_and_blob(self, rel, op):
        """Drop the Index row for `rel` (any kind), evicting a file's pool blob too.
        Dirs/symlinks/whiteouts have no on-disk artifact — the row is the change."""
        if self.idx is None: return
        rid = (self.idx.row_id(rel)
               if self.idx.kind_of(rel) == "file" else None)
        self.idx.del_entry(rel, op=op)
        if rid is not None:
            try: blob_path(self.idx.box_id, rid).unlink()
            except OSError: pass

    def remove(self, rel):
        self._drop_index_row_and_blob(rel.lstrip("/"), "applied")

    def discard(self, rel):
        """Drop a change without writing the host (handles whiteouts in the Index)."""
        self._drop_index_row_and_blob(rel.lstrip("/"), "discarded")

    def settle(self, rel):
        """If the overlay copy no longer differs from the host (or a created file has
        been emptied), drop it from the overlay. Only regular files settle; dirs/
        symlinks/whiteouts are explicit decisions kept until applied/discarded."""
        rel = rel.lstrip("/")
        if self.idx is None or self.idx.kind_of(rel) != "file": return
        cur = self.current_bytes(rel)
        if cur is None: return
        dst = Path("/") / rel
        try:
            settled = (cur == dst.read_bytes()) if dst.exists() else (len(cur) == 0)
        except OSError:
            return
        if settled:
            self._drop_index_row_and_blob(rel, "settled")

    def write_current(self, rel, data):
        """Revert bytes into the live file's pool blob (for discard_hunk). The blob is
        the authoritative file rest form — write it through the Index row."""
        rel = rel.lstrip("/")
        if self.idx is None or self.idx.kind_of(rel) != "file":
            return dict(ok=False, error="not a regular file in the overlay")
        rid = self.idx.row_id(rel)
        if rid is None:
            return dict(ok=False, error="missing row for file")
        bp = blob_path(self.idx.box_id, rid)
        try:
            bp.parent.mkdir(parents=True, exist_ok=True)
            bp.write_bytes(data)
            # If the file was evicted (bytes in the row), the blob is now the truth —
            # NULL the row so it can't shadow the new content.
            self.idx.mark_resident(rel)
        except OSError as e:
            return dict(ok=False, error=str(e))
        return None

    def apply_plan(self, rel):
        # Plan straight from the mirror/row/pool — no on-disk dir/symlink artifact.
        rel = rel.lstrip("/")
        if self.idx is None:
            return dict(kind="noop")
        kind = self.idx.kind_of(rel)
        # A deletion may come from a whiteout or an opaque parent dir (entries()).
        if kind == "whiteout" or self._kinds.get(rel) == "deleted":
            return dict(kind="delete")
        if kind == "symlink":
            tgt = self.idx.symlink_target(rel) or b""
            return dict(kind="symlink",
                        target=bytes(tgt).decode("utf-8", "surrogateescape"),
                        mtime_ns=self.idx.row_mtime(rel) or None)
        if kind == "dir":
            return dict(kind="dir")
        if kind == "file":
            row = self.idx.file_row(rel)   # (rowid, mode, sz, mtime, data)
            if row is not None:
                rid, mode, sz, mtime, data = row
                if data is None:
                    bp = blob_path(self.idx.box_id, rid)
                    if bp.exists():
                        return dict(kind="file", copy_from=bp,
                                    chmod=stat_mod.S_IMODE(mode),
                                    mtime_ns=mtime or None)
                    # Resident row with no pool blob: bytes unreadable.  Return None
                    # (not "noop") so callers treat this the same as SqlarArchive's
                    # missing-blob case — fail closed rather than silently skip.
                    return None
                else:
                    # evicted: bytes live in the row — hand them directly.
                    blob = bytes(data)
                    content = blob if len(blob) == sz else zlib.decompress(blob)
                    return dict(kind="file", data=content,
                                chmod=stat_mod.S_IMODE(mode),
                                mtime_ns=mtime or None)
        return dict(kind="noop")

    @functools.cached_property
    def _kinds(self):
        return {e["path"]: e["kind"] for e in self.entries()}

class SqlarArchive(ChangeSource):
    """Change substrate for a finished box: its single sqlar archive. CHAR-mode rows
    are tombstones (deletions)."""

    def __init__(self, review, sid):
        self.review, self.sid = review, sid
        self.path = sqlar_path(sid)

    def current_bytes(self, rel):
        return sqlar_content(self.path, rel.lstrip("/"))

    def current_mode(self, rel):
        return sqlar_mode(self.path, rel.lstrip("/"))

    def current_mtime(self, rel):
        return sqlar_mtime(self.path, rel.lstrip("/"))

    def entries(self):
        # Cheap O(n) backbone: rows straight from sqlar_list, kind from the stored
        # mode alone — NO host I/O (no exists/lstat, no _is_text_change). is_text /
        # stale / precise created-vs-modified are decorated lazily per visible row.
        out = []
        for name, mode, mtime, sz in sqlar_list(self.path):
            if stat_mod.S_ISCHR(mode): kind = "deleted"
            elif stat_mod.S_ISLNK(mode): kind = "symlink"
            else: kind = "changed"
            out.append(dict(path=name, kind=kind, size=sz))
        return out

    def _drop_row_and_blob(self, rel):
        """Drop the row AND any permanent pool blob backing it (a resident file's bytes
        live only on disk; sqlar_remove alone would orphan them)."""
        bid = box_id_of_sqlar(self.path)
        _drop_sqlar_row_and_blob(self.path, bid, rel)

    def remove(self, rel):
        self._drop_row_and_blob(rel)

    def discard(self, rel):
        self._drop_row_and_blob(rel)

    def settle(self, rel):
        # After a hunk op the diff is gone exactly when the stored current bytes
        # equal the host's bytes; drop the row then.
        rel = rel.lstrip("/")
        if (self.current_bytes(rel) or b"") == ChangeReview._lower_bytes(rel):
            # A resident file keeps its bytes in the pool blob — drop the row AND the
            # blob (sqlar_remove alone would orphan the blob on disk).
            self._drop_row_and_blob(rel)

    def write_current(self, rel, data):
        rel = rel.lstrip("/")
        conn = _sqlar_open(self.path)
        row = None
        try:
            blob, sz = _sqlar_deflate(data)
            conn.execute("UPDATE sqlar SET sz=?, data=? WHERE name=?",
                         (sz, sqlite3.Binary(blob), rel)); conn.commit()
            row = conn.execute("SELECT rowid FROM sqlar WHERE name=?", (rel,)).fetchone()
        finally: conn.close()
        # If this file was a permanent pool file (resident), the row now carries the
        # authoritative bytes — drop the stale blob so it can't shadow the new content
        # (blob_path is checked before the row in _resolve_st).
        bid = box_id_of_sqlar(self.path)
        if bid is not None and row is not None:
            try:
                bp = blob_path(int(bid), row[0])
                if bp.exists(): bp.unlink()
            except (OSError, ValueError, TypeError): pass
        return None

    def apply_plan(self, rel):
        rel = rel.lstrip("/")
        mode, mtime_ns = sqlar_mode_mtime(self.path, rel)  # single SELECT for both
        if mode is None: return None
        if stat_mod.S_ISCHR(mode):               # tombstone → delete
            return dict(kind="delete")
        # The row exists (mode is not None), so the content MUST be readable. A None here
        # means a resident row whose pool blob is missing — do NOT silently coerce it to
        # an empty file (that would write a zero-byte file over the lower). Treat it as
        # "not in archive" so the caller reports an error rather than corrupting data. A
        # genuinely empty file has data non-NULL with sz=0, so sqlar_content returns b"".
        content = sqlar_content(self.path, rel)
        if content is None:
            return None
        if stat_mod.S_ISLNK(mode):
            tgt = content.decode("utf-8", "surrogateescape")
            return dict(kind="symlink", target=tgt, mtime_ns=mtime_ns)
        return dict(kind="file", data=content,
                    chmod=stat_mod.S_IMODE(mode), mtime_ns=mtime_ns)

def fmt_bytes(n) -> str:
    """Human byte size (1.5M / 3.0K / 42B). Module-level so non-UI callers
    (ChangeReview.structural_diff_quick) can use it too — the UI closes over it."""
    for u, d in (("G", 1 << 30), ("M", 1 << 20), ("K", 1 << 10)):
        if n >= d: return f"{n/d:.1f}{u}"
    return f"{n}B"

class ChangeReview:
    """The box-change review model over the ChangeSource barrier. Lists, diffs,
    applies, and discards a box's changes (whole-file and per-hunk) and computes
    its on-demand patch; routes every operation through the one ChangeSource that
    `_source(sid)` selects (live in-RAM Index vs the box's at-rest sqlar).
    Holds one back-reference `self.reg` to the owning Supervisor registry, for the
    session set (`reg.sessions`/`reg.indexes`) and registry callbacks
    (`reg._reap_empty`/`reg._emit`/`reg.delete`). Owns the per-run
    consolidation memo (`_consolidated`)."""

    def __init__(self, reg: "Supervisor"):
        self.reg = reg
        self._consolidated: set = set()   # sids already consolidated this run
        # sids whose consolidate() is running OFF the loop in a worker thread. While a
        # sid is here the UI shows a "Consolidating…" placeholder and never reads its
        # not-yet-complete change set. Mutated ONLY on the UI loop (start/finish), so
        # it stays single-thread-confined like _consolidated.
        self._consolidating: set = set()
        # Rendered structural-diff text per (sid, rel) for the binary detail pane
        # (lazy: only filled when that single entry's detail is shown). Invalidated
        # wherever the other change caches are.
        self._struct_cache: dict = {}

    def _entries(self, sid) -> list:
        """All changed entries for a session — enumeration is delegated to the box's
        ChangeSource; stale/is_text/kind decoration lives inside it."""
        return self._source(sid).entries()

    def _is_text_change(self, src, rel) -> bool:
        """Whether the (base, current) pair is a *text* change — i.e. a line-based
        unified diff applied with patch(1) would reproduce the current bytes exactly.
        Conservative approximation of that: both sides must be NUL-free (a NUL is
        what breaks patch byte-exactness); tombstones, symlinks and directories are
        never text.  Examines BOTH files of the pair, not just the current one."""
        rel = rel.lstrip("/")
        mode = src.current_mode(rel)
        if mode is None or stat_mod.S_ISCHR(mode) or stat_mod.S_ISLNK(mode):
            return False
        cur = src.current_bytes(rel)
        if cur is None or b"\x00" in cur: return False
        return b"\x00" not in self._lower_bytes(rel)

    def _hunks(self, src, rel):
        """Unified (lower_lines, upper_lines, groups) from `src`, or None for a
        non-text change.  Source-agnostic: reads via the ChangeSource only."""
        rel = rel.lstrip("/")
        if not self._is_text_change(src, rel): return None
        ll = ut_split(self._lower_bytes(rel)); ul = ut_split(src.current_bytes(rel))
        groups = list(difflib.SequenceMatcher(None, ll, ul).get_grouped_opcodes(3))
        return (ll, ul, groups)

    def _hunks_display(self, src, rel) -> dict:
        """Build the hunks display dict via _hunks (source-agnostic)."""
        rel = rel.lstrip("/")
        info = self._hunks(src, rel)
        if info is not None:
            ll, ul, groups = info
            return dict(is_text=True, hunks=_build_hunks_display(ll, ul, groups))
        # non-text / deleted / symlink / binary: build a display-only diff summary.
        mode = src.current_mode(rel)
        host = Path("/") / rel
        if mode is None:
            return dict(is_text=False, hunks=[], diff=dict(kind="error", error="gone"))
        if stat_mod.S_ISCHR(mode):
            return dict(is_text=False, hunks=[], diff=dict(kind="deleted"))
        if stat_mod.S_ISLNK(mode):
            tgt = src.current_bytes(rel) or b""
            d = dict(kind="modified" if host.is_symlink() else "created",
                     diff="symlink → " + tgt.decode("utf-8", "replace"))
            return dict(is_text=False, hunks=[], diff=d)
        raw = src.current_bytes(rel) or b""
        kind = "modified" if host.exists() else "created"
        d = dict(kind=kind, content=base64.b64encode(raw).decode())
        if kind == "modified":
            before = self._lower_bytes(rel)
            if before:
                d["content_before"] = base64.b64encode(before).decode()
        return dict(is_text=False, hunks=[], diff=d)

    def _consolidate_prep(self, sid) -> "dict | None":
        """Loop-thread half of consolidation: decide whether `sid` (a finished,
        not-yet-consolidated box) needs folding and, if so, mark it consolidated and
        close + drop its live Index handle (a finished box has none live, but a stale
        handle may linger). Returns the off-loop work args (shm_dir) or None when
        there is nothing to do. Mutates registry state, so it MUST run on the UI loop;
        the heavy consolidate() it enables runs OFF the loop (sync here, worker for
        the UI)."""
        if sid in self._consolidated: return None
        s = self.reg.sessions.get(sid)
        if not s or _box_running(s): return None
        self._consolidated.add(sid)
        if not (s.shm_dir and Path(s.shm_dir).exists()): return None
        idx = self.reg.indexes.pop(sid, None)
        if idx is not None:
            try: idx.close()
            except Exception: pass
        return dict(shm_dir=s.shm_dir)

    def _consolidate_finish(self, sid) -> None:
        """Loop-thread half: post-consolidate registry bookkeeping (has_sqlar refresh,
        free a spent backing tree). MUST run on the UI loop. Idempotent."""
        s = self.reg.sessions.get(sid)
        if not s: return
        s.has_sqlar = sqlar_nonempty(sqlar_path(sid))
        try: empty = not upper_has_changes(Path(s.upper))
        except OSError: empty = False
        if empty and s.shm_dir:
            rm_rf(s.shm_dir)

    def _ensure_consolidated(self, sid) -> None:
        """Consolidate a finished session's overlay into its single sqlar the first
        time we look at it (for orphaned sessions left on disk by a crashed runner).
        Never touches a live/running session. SYNCHRONOUS — for the control plane
        (patch) and post-edit callers; the interactive UI uses the off-loop worker
        path (UI._start_consolidate) so the loop never blocks."""
        args = self._consolidate_prep(sid)
        if args is None: return
        try:
            consolidate(args["shm_dir"], sid)
        except Exception:
            return
        self._consolidate_finish(sid)

    def invalidate_consolidation(self) -> None:
        """Forget which sessions were already consolidated this run, so each gets
        a fresh consolidate pass on next view (e.g. after a file-rule change)."""
        self._consolidated.clear()
        self._struct_cache.clear()

    # ─── source-agnostic change interface ──────────────────────────────────
    def session_changes(self, sid) -> list:
        """All changes for a session, read straight from the box's own store. In the
        per-file model there is no separate "finished form" to build first: every entry
        is written into the box's single sqlar as it happens (file rows resident with
        bytes in the pool, dirs/symlinks/whiteouts carried in the row), so a stopped box
        is ALREADY at rest. Reads therefore go directly to the rows — NO consolidate pass
        on the loop. (The one-time fold of an overlay still happens at box exit and in the
        startup orphan sweep; consolidate is recovery/placement, not a read gate.) While a
        box is mid-fold (`_consolidating`) its partial state is off-limits: refuse."""
        if sid in self._consolidating: return []
        return self._entries(sid)

    def _live(self, sid) -> bool:
        s = self.reg.sessions.get(sid)
        return bool(_box_running(s) and Path(s.upper).exists())

    def _source(self, sid) -> ChangeSource:
        """THE sole live-vs-consolidated branch in the change-review code: a live box
        reads/writes its in-RAM Index (canonical), a finished one its sqlar."""
        return LiveUpper(self, sid) if self._live(sid) else SqlarArchive(self, sid)

    @staticmethod
    def _lower_bytes(rel) -> bytes:
        """Original host content for /<rel> as bytes; b'' if missing or a directory.
        This is the diff base, identical for both live and consolidated states."""
        host = Path("/") / rel.lstrip("/")
        try:
            if host.exists() and not host.is_dir(): return host.read_bytes()
        except OSError: pass
        return b""

    @staticmethod
    def _write_host_change(host: Path, plan: dict) -> "dict | None":
        """Materialise one change onto the host per `plan` (a ChangeSource.apply_plan
        result). Source-agnostic; dispatches on plan kind, never on live-vs-sqlar.
        Returns dict(ok=False, error=...) to abort, or None on success."""
        kind = plan["kind"]
        if kind == "delete":
            if host.is_dir() and not host.is_symlink(): shutil.rmtree(host)
            elif host.exists() or host.is_symlink(): host.unlink()
        elif kind == "symlink":
            if host.exists() or host.is_symlink(): host.unlink()
            host.parent.mkdir(parents=True, exist_ok=True); os.symlink(plan["target"], host)
            _restore_host_mtime_ns(host, plan.get("mtime_ns"), is_symlink=True)
        elif kind == "dir":
            host.mkdir(parents=True, exist_ok=True)
        elif kind == "file":
            if _is_symlink(host):
                return dict(error="refusing to write through a symlink")
            host.parent.mkdir(parents=True, exist_ok=True)
            if "copy_from" in plan:
                shutil.copy2(plan["copy_from"], host)   # carries ns mtime from the upper
            else:
                host.write_bytes(plan["data"])
                if plan.get("chmod") is not None:
                    try: os.chmod(host, plan["chmod"])
                    except OSError: pass
                _restore_host_mtime_ns(host, plan.get("mtime_ns"), is_symlink=False)
        return None

    def _parent_key(self, sid: str) -> "str | None":
        """Return the parent box's key str(parent_box_id) if this box is nested and the
        parent is a known session, else None. Used by the apply paths to decide whether
        to write the host or promote into the parent's overlay."""
        s = self.reg.sessions.get(sid)
        if s is None: return None
        if s.parent_box_id is None: return None
        psid = str(s.parent_box_id)
        # Only promote if the parent session is still tracked (live or finished).
        return psid if psid in self.reg.sessions else None

    def _raw_parent_key(self, sid: str) -> "str | None":
        """The parent box key of `sid` from the Session (if tracked) else the sqlar
        'parent_box_id' meta — the RAW parent chain, including parents that are not
        currently registered as sessions. None / "" means a top-level box. This is
        the chain _lower_has walks to the host; unlike _parent_key it does NOT
        require the parent to be a live/tracked session."""
        s = self.reg.sessions.get(sid)
        if s is not None and s.parent_box_id is not None:
            return str(s.parent_box_id)
        psid = sqlar_meta_get(sqlar_path(sid), "parent_box_id")
        return psid or None

    def _own_kind(self, sid: str, rel: str) -> "str | None":
        """`sid`'s OWN entry kind for `rel` (file/symlink/dir/whiteout) or None if the
        box has no own entry. Reads the live Index mirror if open, else the sqlar mode.
        This is the box's overlay decision only — it does NOT chain to the lower."""
        rel = rel.lstrip("/")
        idx = self.reg.indexes.get(sid)
        if idx is not None:
            return idx.kind_of(rel)
        mode = sqlar_mode(sqlar_path(sid), rel)
        if mode is None:
            return None
        return _kind_of_mode(mode)

    def _lower_has(self, sid: str, rel: str) -> bool:
        """Does sid's LOWER (what it inherits, ignoring sid's own overlay) currently
        resolve `rel` to a PRESENT entry? Walks the parent chain to the host:
          - no parent (top-level box): whether the host path exists or is a symlink;
          - has parent p: inspect p's OWN entry — a tombstone/whiteout means the parent
            deleted it (False); a real present entry (file/symlink/dir) means True; if p
            has no own entry, recurse into p's lower.
        This generalizes the old hardcoded Path("/")/rel host check to nested boxes.
        A `seen` set and depth cap (matching display_path) guard against circular
        parent_box_id chains."""
        rel = rel.lstrip("/")
        cur = sid
        seen: set = set()
        for _ in range(64):
            psid = self._raw_parent_key(cur)
            if not psid:
                host = Path("/") / rel
                return host.exists() or host.is_symlink()
            if psid in seen:
                return False        # cycle in parent chain: stop safely
            seen.add(psid)
            k = self._own_kind(psid, rel)
            if k == "whiteout":
                return False
            if k is not None:
                return True
            cur = psid
        return False                # depth exceeded: treat as not found

    def _promote_into_parent(self, target_sid: str, rel: str,
                             plan: dict) -> "dict | None":
        """Capture the change described by `plan` into the TARGET box's overlay as a new
        pending change — instead of writing to the real host. The target is the PARENT
        box for an 'apply', or an immediate CHILD box for a 'discard' copy-down
        (preserving the child's inherited view). Returns None on success, dict(ok=False,
        error=…) on failure.

        ONE code path, keyed by the target box's own storage (its <box_id>.sqlar row plus
        a pool blob for large file bytes) — it does NOT branch on whether the box has a
        live in-RAM Index. If a live Index IS open for the target, the write additionally
        keeps that RAM mirror consistent so a running FUSE mount keeps serving the new
        value. The RAM mirror is an accelerator, never separate semantics."""
        rel = rel.lstrip("/")
        kind = plan.get("kind", "noop")
        if kind == "noop":
            return None

        idx = self.reg.indexes.get(target_sid)   # live Index, or None
        sp = sqlar_path(target_sid)
        mtime_ns = plan.get("mtime_ns") or int(time.time() * 1e9)
        try:
            box_id = idx.box_id if idx is not None else as_box_id(target_sid)
        except Exception as e:
            return dict(ok=False, error=f"promote: box id: {e}")
        # Best-effort writer id for the live mirror (any attribution is fine).
        wid = 0
        if idx is not None:
            try: wid = idx.writer_for(os.getpid())
            except Exception: wid = 0

        try:
            if kind == "delete":
                # Tombstone iff the target's own LOWER still resolves rel to a present
                # entry (so the deletion must shadow it); otherwise just drop the box's
                # own row. This walks the box's PARENT chain — NOT the real host — fixing
                # the old bug where the live branch checked Path("/")/rel directly.
                if self._lower_has(target_sid, rel):
                    if idx is not None:
                        idx.set_entry(rel, "whiteout", 0, wid, "promoted")
                    else:
                        conn = _sqlar_open(sp)
                        try: _sqlar_put_tombstone(conn, rel); conn.commit()
                        finally: conn.close()
                else:
                    # Lower has nothing here: drop the box's own row + any pool blob.
                    if idx is not None:
                        # Drop the row FIRST, then the blob (Stage A ordering: never
                        # leave a resident row whose blob is gone).
                        rid = idx.row_id(rel)
                        idx.del_entry(rel, op="promoted_del")
                        if rid is not None:
                            try: blob_path(box_id, rid).unlink()
                            except OSError: pass
                    else:
                        _drop_sqlar_row_and_blob(sp, box_id, rel)
            elif kind == "symlink":
                tgt_raw = plan.get("target", "")
                if isinstance(tgt_raw, bytes):
                    tgt_str = tgt_raw.decode("utf-8", "surrogateescape")
                else:
                    tgt_str = str(tgt_raw)
                tgt_bytes = tgt_str.encode("utf-8", "surrogateescape")
                if idx is not None:
                    # Live box: the FUSE readlink/serve path resolves a symlink straight
                    # from the row (target in the row, no on-disk artifact). Write the row
                    # through the lock-held Index so the running mount serves it. (Single
                    # Index connection — no second WAL handle racing the serve thread.)
                    idx.set_entry(rel, "symlink", stat_mod.S_IFLNK | 0o777, wid, "promoted",
                                  target=tgt_bytes, mtime_ns=mtime_ns)
                else:
                    # At rest: the target bytes live in the sqlar row itself.
                    conn = _sqlar_open(sp)
                    try: _sqlar_put_symlink(conn, rel, tgt_bytes, mtime_ns); conn.commit()
                    finally: conn.close()
            elif kind == "file":
                data = plan.get("data")
                if data is None and "copy_from" in plan:
                    try: data = Path(plan["copy_from"]).read_bytes()
                    except OSError as e:
                        return dict(ok=False, error=f"promote read blob: {e}")
                if data is None:
                    return dict(ok=False, error="promote: no file data in plan")
                mode = plan.get("chmod") or (stat_mod.S_IFREG | 0o644)
                if not stat_mod.S_ISREG(mode):
                    mode = stat_mod.S_IFREG | stat_mod.S_IMODE(mode)
                if idx is not None:
                    # Live box: a file's bytes live in the pool blob (the serve path reads
                    # blob_path first); the row stays resident (data NULL). Upsert the row
                    # through the lock-held Index, then write the blob — exactly as the
                    # FUSE write path records a new file.
                    idx.set_entry(rel, "file", mode, wid, "promoted")
                    rid = idx.row_id(rel)
                    if rid is None:
                        return dict(ok=False, error="promote: row_id missing after set_entry")
                    bp = blob_path(box_id, rid)
                    try:
                        bp.parent.mkdir(parents=True, exist_ok=True)
                        bp.write_bytes(data)
                    except OSError as e:
                        # Roll back the row so we never leave a resident row whose
                        # blob is missing (mirrors the at-rest branch's rollback).
                        try: idx.del_entry(rel, op="promoted_blob_fail")
                        except Exception: pass
                        return dict(ok=False, error=f"promote write blob: {e}")
                else:
                    # At rest: size-based placement matching consolidate /
                    # POOL_RESIDENT_MIN — a large file stays a PERMANENT pool blob
                    # (resident row: data NULL, real sz), a small file folds inline.
                    if len(data) >= POOL_RESIDENT_MIN:
                        conn = _sqlar_open(sp)
                        try:
                            conn.execute(
                                "INSERT INTO sqlar(name,mode,mtime,sz,data) VALUES(?,?,?,?,NULL)"
                                " ON CONFLICT(name) DO UPDATE SET mode=excluded.mode,"
                                " mtime=excluded.mtime, sz=excluded.sz, data=NULL",
                                (rel, int(mode), int(mtime_ns), len(data)))
                            conn.commit()
                            rid = conn.execute("SELECT rowid FROM sqlar WHERE name=?",
                                               (rel,)).fetchone()[0]
                        finally:
                            conn.close()
                        bp = blob_path(box_id, rid)
                        try:
                            bp.parent.mkdir(parents=True, exist_ok=True)
                            bp.write_bytes(data)
                        except OSError as e:
                            # The row was committed but the blob was never written.
                            # Delete the row now so we never leave a resident row
                            # whose blob is missing (would read back as empty).
                            try: sqlar_remove(sp, rel)
                            except Exception: pass
                            return dict(ok=False, error=f"promote write blob: {e}")
                    else:
                        conn = _sqlar_open(sp)
                        try: _sqlar_put_file(conn, rel, data, mode, mtime_ns); conn.commit()
                        finally: conn.close()
        except sqlite3.Error as e:
            return dict(ok=False, error=f"promote to sqlar: {e}")
        return None   # success

    # ── discard copy-down: pin a file into children before dropping it ─────────
    def _immediate_children(self, sid: str) -> list:
        """Direct child boxes of `sid` (parent_box_id == sid's box_id): live sessions
        plus finished sqlar-only boxes (parent recorded in 'parent_box_id' meta)."""
        kids = set()
        for c, m in self.reg._all_box_meta().items():
            if c != sid and m["parent_box_id"] == sid:
                kids.add(c)
        return sorted(kids, key=lambda k: int(k) if BOX_ID_RE.match(k) else 0)

    def _box_has_own_entry(self, child: str, rel: str) -> bool:
        """True if `child` already resolves `rel` itself (its own file/symlink/dir/
        whiteout), so a copy-down must NOT override the child's explicit decision."""
        rel = rel.lstrip("/")
        idx = self.reg.indexes.get(child)                 # live Index, or None
        if idx is not None:
            return idx.kind_of(rel) is not None
        return sqlar_mode(sqlar_path(child), rel) is not None

    def _copydown_to_children(self, sid: str, rel: str, src) -> "dict | None":
        """Before dropping `rel` from `sid`, pin its current value into every immediate
        child that inherits it (has no own entry) — so discarding from `sid` never
        changes a child's merged view (the child keeps the value it was seeing). A
        deletion (whiteout) copies down as a whiteout, preserving an 'absent' view.
        Returns None on success; dict(ok=False, error=…) if any child copy-down failed
        (the caller MUST NOT then drop `rel` from `sid` — the child would lose its
        inherited view)."""
        kids = self._immediate_children(sid)
        if not kids:
            return None
        plan = src.apply_plan(rel)
        if plan is None:
            # Source claims this path but its bytes can't be read (e.g. a resident row
            # whose blob is gone): fail closed rather than copy down an empty file.
            return dict(ok=False, error=f"copy-down: {rel} not readable from source")
        if plan.get("kind") == "noop":
            return None
        for child in kids:
            if self._box_has_own_entry(child, rel):
                continue
            err = self._promote_into_parent(child, rel, plan)   # capture into the child
            if err is not None:
                return dict(ok=False,
                            error=f"copy-down into {child}: {err.get('error', err)}")
        return None

    def finalize_by_rules(self, sid) -> dict:
        """Run the file rules over a stopped box's whole change set, the way box DELETE
        wants it: an 'apply'-matched path is promoted (parent overlay / host); every
        other path (a 'discard' rule, 'passthrough', or no rule at all) is discarded —
        which copies it down into immediate children that lack it. No-op for a box with
        no changes.

        Reads the change set straight from the box's store — NO consolidate pass (a
        stopped box is already at rest; see session_changes). The per-row discard is the
        only step that touches the box's storage one entry at a time, and its ONLY
        externally-visible effect is the copy-down into immediate children; with no child
        to inherit, it is skipped here and dissolve()'s _free_storage drops the whole
        archive (sqlar + pool) in bulk — avoiding a commit + emptiness re-check per row
        over a large box. After this returns, the box holds at most the (now harmless)
        discard rows, which dissolve frees immediately."""
        changes = [e["path"] for e in self.session_changes(sid)]
        if not changes:
            return dict(applied=[], discarded=[], errors=[])
        frules = load_file_rules()
        try: box = self.reg.display_path(sid)
        except Exception: box = ""
        sp = sqlar_path(sid)
        apply_paths, discard_paths = [], []
        for rel in changes:
            proc = sqlar_first_writer_prov(sp, rel)   # match the path's FIRST writer
            (apply_paths if frules.decide(rel, box, proc) == "apply"
             else discard_paths).append(rel)
        a = self.apply(sid, apply_paths, reap=False) if apply_paths \
            else dict(applied=[], errors=[])
        # Per-row discard exists only to copy paths DOWN into immediate children. With no
        # children, dropping each row (a commit + an emptiness re-check apiece) is pure
        # churn — _free_storage in dissolve() removes the whole archive right after. So
        # only pay the per-row cost when a child actually needs the copy-down.
        if discard_paths and self._immediate_children(sid):
            d = self.discard(sid, discard_paths, reap=False)
        else:
            d = dict(discarded=discard_paths, errors=[])
        return dict(applied=a.get("applied", []), discarded=d.get("discarded", []),
                    errors=a.get("errors", []) + d.get("errors", []))

    def change_mode(self, sid, rel, source=None) -> "int | None":
        """st_mode for a changed file (for the path/mode header). None if N/A."""
        return self._source(sid).current_mode(rel)

    def decorate(self, sid, rel, source=None) -> dict:
        """The per-row lazy decoration for ONE changed entry — the host-touching work
        kept out of the cheap entries() backbone, scoped to a single rel: is_text (NUL
        pair-wise rule), stale (host-vs-stored mtime), and kind refined to created vs
        modified via a single host stat. Pass `source` to reuse one ChangeSource across
        a visible window. Cheap (a couple of syscalls + one file pair)."""
        rel = rel.lstrip("/")
        src = source if source is not None else self._source(sid)
        mode = src.current_mode(rel)
        if mode is None:
            return dict(is_text=False, stale=False, kind="changed")
        host = Path("/") / rel
        if stat_mod.S_ISCHR(mode):
            return dict(is_text=False, stale=False, kind="deleted")
        is_text = self._is_text_change(src, rel)
        try: hstat = host.lstat(); exists = True
        except OSError: hstat = None; exists = False
        kind = "modified" if exists else "created"
        stale = False
        if exists and hstat is not None:
            cm = src.current_mtime(rel)              # integer ns (st_mtime_ns)
            if cm is not None: stale = hstat.st_mtime_ns > cm
        return dict(is_text=is_text, stale=stale, kind=kind)

    def hunks(self, sid, rel) -> dict:
        return self._hunks_display(self._source(sid), rel)

    def structural_diff(self, sid, rel) -> "dict | None":
        """READ-ONLY, informational insight for a single binary entry: the libmagic
        type line(s) on top, then (for a recognized type) a unified diff of a
        structural dump of base vs current bytes — readelf for ELF, openssl for
        certs/keys, ar/unzip/tar for archives. SYNCHRONOUS (runs the sandboxed parsers
        inline); the interactive UI uses the quick/finish split below so it never
        blocks. Returns dict(lines=...) or None when magic is unavailable."""
        lines, job = self.structural_diff_quick(sid, rel)
        if job is not None:
            return self.structural_diff_finish(job)
        return self._struct_cache.get((sid, rel.lstrip("/")))

    def _struct_type(self, data: bytes) -> str:
        try: return magic.from_buffer(data[:65536]) or "data"
        except Exception: return "data"

    def structural_diff_quick(self, sid, rel) -> tuple:
        """FAST in-process half: libmagic type line(s) + differ selection — no
        sandboxed parser runs here. Returns (lines, job): when `job` is None the lines
        are the COMPLETE result (already cached: a hit, an unrecognized type, or a file
        over the size cap); when `job` is a dict it describes the heavyweight dump for
        structural_diff_finish to run off the UI loop. `lines` is the partial result to
        show immediately (type + the structural-diff header). Never raises."""
        rel = rel.lstrip("/")
        key = (sid, rel)
        if key in self._struct_cache:
            res = self._struct_cache[key]
            return ((res.get("lines", []) if res else []), None)
        if magic is None:
            self._struct_cache[key] = None
            return ([], None)
        try:
            src = self._source(sid)
            base = self._lower_bytes(rel) or b""
            cur = src.current_bytes(rel) or b""
        except Exception as e:
            res = dict(lines=[("err", f"structural diff failed: {e}")])
            self._struct_cache[key] = res
            return (res["lines"], None)
        lines = []
        if base and cur:
            lines.append(("type", f"type (base): {self._struct_type(base)}"))
            lines.append(("type", f"type (current): {self._struct_type(cur)}"))
        else:
            lines.append(("type", f"type: {self._struct_type(cur or base)}"))
        sniff = cur or base
        diff = differ_for(self._struct_type(sniff), sniff)
        if diff is None:                                  # nothing heavy to run
            self._struct_cache[key] = dict(lines=lines)
            return (lines, None)
        argv, label = diff
        lines.append(("hdr", f"── structural diff · {label} ──"))
        if (base and len(base) > _STRUCT_MAX) or (cur and len(cur) > _STRUCT_MAX):
            lines.append(("dim", f"(skipped: file exceeds {fmt_bytes(_STRUCT_MAX)})"))
            self._struct_cache[key] = dict(lines=lines)
            return (lines, None)
        return (lines, dict(key=key, argv=argv, base=base, cur=cur, head=list(lines)))

    def structural_diff_finish(self, job, on_spawn=None) -> dict:
        """SLOW half: run the sandboxed dump(s) for `job` (from structural_diff_quick)
        and build the unified structural diff. Meant to run OFF the UI loop. Each bwrap
        Popen is handed to `on_spawn(proc)` (if given) so the caller can kill it to
        cancel. Caches and returns the final dict(lines=...). Never raises."""
        lines = list(job["head"]); base, cur, argv = job["base"], job["cur"], job["argv"]
        try:
            def dump(data):
                if not data: return ""
                ok, out, err = run_on_untrusted(argv, {"in": data}, on_spawn=on_spawn)
                return out if ok else f"<parser error: {err}>"
            if base and cur:
                bd, cd = dump(base), dump(cur)
                any_line = False
                for ln in difflib.unified_diff(bd.splitlines(), cd.splitlines(),
                                               "base", "current", lineterm=""):
                    any_line = True
                    if ln.startswith(("+++", "---")): lines.append(("hdr", ln))
                    elif ln.startswith("+"): lines.append(("+", ln))
                    elif ln.startswith("-"): lines.append(("-", ln))
                    elif ln.startswith("@@"): lines.append(("@", ln))
                    else: lines.append((" ", ln))
                if not any_line:
                    lines.append(("dim", "(structural dumps identical)"))
            else:
                lines.append(("dim", f"({'current' if cur else 'base'} only)"))
                for ln in dump(cur or base).splitlines():
                    lines.append((" ", ln))
        except Exception as e:
            lines.append(("err", f"structural diff failed: {e}"))
        res = dict(lines=lines)
        self._struct_cache[job["key"]] = res
        return res

    def invalidate_struct(self, sid=None) -> None:
        """Drop cached structural diffs (all, or just one sid). Called wherever the
        other change caches are invalidated."""
        if sid is None: self._struct_cache.clear()
        else:
            for k in [k for k in self._struct_cache if k[0] == sid]:
                self._struct_cache.pop(k, None)

    def apply_hunk(self, sid, rel, index) -> dict:
        # Apply one hunk onto the host; the change already contains it, so that hunk
        # simply stops being a difference.  Byte-exact splice on raw byte-lines.
        src = self._source(sid)
        info = self._hunks(src, rel)
        if info is None: return dict(ok=False, error="not a text change")
        ll, ul, groups = info
        if not (0 <= index < len(groups)): return dict(ok=False, error="stale hunk")
        g = groups[index]
        a1, a2, b1, b2 = g[0][1], g[-1][2], g[0][3], g[-1][4]
        new_lower = ll[:a1] + ul[b1:b2] + ll[a2:]
        result = _write_host_hunk(Path("/") / rel.lstrip("/"), new_lower)
        if not result["ok"]: return result
        src.settle(rel)
        self.reg._reap_empty(sid)   # deferred: a hunk op may have emptied the box
        return dict(ok=True)

    def discard_hunk(self, sid, rel, index) -> dict:
        # Revert one hunk in the change (back to the host's bytes at that range).
        src = self._source(sid)
        info = self._hunks(src, rel)
        if info is None: return dict(ok=False, error="not a text change")
        ll, ul, groups = info
        if not (0 <= index < len(groups)): return dict(ok=False, error="stale hunk")
        g = groups[index]
        a1, a2, b1, b2 = g[0][1], g[-1][2], g[0][3], g[-1][4]
        new_upper = ul[:b1] + ll[a1:a2] + ul[b2:]
        err = src.write_current(rel, b"".join(new_upper))
        if err is not None: return err
        src.settle(rel)
        self.reg._reap_empty(sid)   # deferred: a hunk op may have emptied the box
        return dict(ok=True)

    def patch_text(self, sid) -> bytes:
        """The unified patch for a box, computed ON DEMAND.  Text changes become real
        unified hunks; binary changes get a `Binary files … differ` line (visible +
        counted by diffstat); symlinks stay a comment line (a patch can't carry them)."""
        src = self._source(sid)
        out = []
        for e in self.session_changes(sid):
            rel = e["path"]
            if e["kind"] == "symlink":
                out.append(f"# symlink change: {rel}\n".encode()); continue
            info = self._hunks(src, rel)
            if info is not None:
                ll, ul, groups = info
                created = not (Path("/") / rel).exists()
                fp = build_file_patch(rel, ll, ul, created, False)
                out.append(serialize_patch({rel: fp}))
            elif e["kind"] == "deleted":
                # A deletion has no current bytes (so _hunks is None); classify by the
                # *base* being removed: a text file gets a real removal hunk, a binary
                # file the diff-style "Binary files … differ" line.
                base = self._lower_bytes(rel)
                if b"\x00" in base:
                    eb = rel.encode("utf-8", "surrogateescape")
                    out.append(b"Binary files a/" + eb + b" and /dev/null differ\n")
                else:
                    fp = build_file_patch(rel, ut_split(base), [], False, True)
                    out.append(serialize_patch({rel: fp}))
            else:
                # Binary create/modify: standard diff-style line where hunks would go.
                eb = rel.encode("utf-8", "surrogateescape")
                if not (Path("/") / rel.lstrip("/")).exists():
                    a, b = b"/dev/null", b"b/" + eb
                else:
                    a, b = b"a/" + eb, b"b/" + eb
                out.append(b"Binary files " + a + b" and " + b + b" differ\n")
        return b"".join(out)

    def apply(self, sid, paths, reap: bool = True) -> dict:
        # Apply == promote into the parent overlay (nested) or write the host (root).
        if paths is None: paths = [e["path"] for e in self.session_changes(sid)]
        src = self._source(sid)
        parent_sid = self._parent_key(sid)   # None for root boxes
        applied, errors = [], []
        for rel in paths:
            host = Path("/") / rel.lstrip("/")
            try:
                plan = src.apply_plan(rel)
                if plan is None:
                    errors.append({"path": rel, "error": "not in archive"}); continue
                if parent_sid is not None:
                    # Nested box: promote into the parent's overlay, not the host.
                    err = self._promote_into_parent(parent_sid, rel, plan)
                else:
                    err = self._write_host_change(host, plan)
                if err is not None: errors.append({"path": rel, **err}); continue
                src.remove(rel)
                applied.append(rel)
            except OSError as e:
                errors.append({"path": rel, "error": str(e)})
        # Deferred, top-level reap: remove the box if this emptied it. Suppressed
        # (reap=False) when called as a sub-step of finalize_by_rules, so finalize never
        # has the box deleted out from under its second (discard) pass.
        if reap: self.reg._reap_empty(sid)
        return dict(applied=applied, errors=errors)

    def discard(self, sid, paths, reap: bool = True) -> dict:
        # Drop each change from the box without writing the host — but first copy it
        # DOWN into any immediate child that inherits it, so the child's merged view is
        # unchanged (this is what lets a non-empty box with children be deleted).
        if paths is None: paths = [e["path"] for e in self.session_changes(sid)]
        src = self._source(sid)
        discarded, errors = [], []
        for rel in paths:
            try:
                cd = self._copydown_to_children(sid, rel, src)
                if cd is not None:
                    # Copy-down failed: do NOT drop rel — a child still inherits it.
                    errors.append({"path": rel, **cd}); continue
                src.discard(rel)
                discarded.append(rel)
            except OSError as e:
                errors.append({"path": rel, "error": str(e)})
        # Deferred top-level reap (suppressed within finalize_by_rules; see apply()).
        if reap: self.reg._reap_empty(sid)
        return dict(discarded=discarded, errors=errors)

class Supervisor:
    """The session registry: owns the live session set (lifecycle, output echo
    plumbing) and composes `self.review` (a ChangeReview). Concurrency contract
    (single-thread-confined, like Index's): EVERY method here runs on the UI
    process's one asyncio main-loop thread. Callers reach it, both on that loop:

      • the control plane — `register`/`unregister`/`drop`/`patch` via
        ChannelServer._dispatch_control, on the asyncio loop;
      • the Textual UI handlers — `kill`/`delete` here, change review via
        `self.review` (`apply`/`discard`/`apply_hunk`/`discard_hunk`/
        `session_changes`/`hunks`/`invalidate_consolidation`/…) — Textual runs on
        the SAME asyncio loop.

    Change-review state (the live-vs-sqlar ChangeSource selection and the
    consolidation memo `self.review._consolidated`) lives on `self.review`.

    Because the control server and the Textual app share one loop and one thread,
    there is no cross-thread access here: `self.sessions` and
    `self.review._consolidated` are single-thread-confined to that loop and
    mutated ONLY from it, so they need no lock.

    The FUSE serve thread (a separate trio thread, see OverlayMount) never
    touches Supervisor state directly. It interacts only through the per-session
    `Index` (the single writer, under Index._lock) and the OverlayMount event
    queue (drained back on the asyncio loop). Keep it that way: any new path
    that would read/write sessions/review state must run on the asyncio
    loop, never on the serve thread."""

    def __init__(self, mount: "OverlayMount | None" = None):
        self.mount = mount               # the single pyfuse3 OverlayMount (UI-owned)
        self.indexes: dict = {}          # sid -> live Index (for running sessions)
        self.sessions: dict = {}
        self.selected_sid = None         # the UI's current box (target of `patch`)
        self.review = ChangeReview(self) # change-review model (owns consolidation memo)
        self.on_event = lambda ev: None
        # HIGH-1: per-session owner tokens, returned to the runner. CLI teardown ops
        # (drop) present it; a box's own persistent connection closing is the box's
        # teardown signal (no token needed there — the connection IS the proof).
        self._owner_tokens: dict = {}     # sid -> random token (kept OUTSIDE the box)
        self._echoes: dict = {}           # sid -> EchoStream (capture-mode live echo)
        self._echo_loop = None            # asyncio loop the box channels run on
        self.sessions.update(discover_sessions())
        # Auto-name counter: a single UI-process integer, seeded once at startup from the
        # highest existing A<n> label, used-then-incremented for each box that launches
        # without a user-supplied name. NO pid, NO timestamp in names.
        self._auto_n = self._seed_auto_n()

    def _seed_auto_n(self) -> int:
        """max(existing A<digits> names)+1, scanning live sessions + on-disk sqlar meta."""
        hi = 0
        rx = re.compile(r"A(\d+)\Z")
        names = [s.name for s in self.sessions.values() if s.name]
        sh = state_home()
        if sh.exists():
            for p in sh.glob("*.sqlar"):
                nm = sqlar_meta_get(p, "name")
                if nm: names.append(nm)
        for nm in names:
            m = rx.match(nm)
            if m:
                try: hi = max(hi, int(m.group(1)))
                except ValueError: pass
        return hi + 1

    def _next_auto_name(self) -> str:
        """Hand out the next A<n> and bump the counter (used-then-incremented)."""
        n = self._auto_n
        self._auto_n += 1
        return "A" + str(n)

    def _all_box_meta(self) -> dict:
        """Enumerate every known box (live sessions + on-disk *.sqlar files whose stem
        matches BOX_ID_RE) and return {key: {"name": str, "parent_box_id": str|None}}.
        Live-session data takes precedence over on-disk sqlar data (same as _box_name /
        _box_parent).  No caching — a fresh scan per call matches current behaviour."""
        meta: dict = {}
        for key, s in self.sessions.items():
            if not BOX_ID_RE.match(key): continue
            meta[key] = {
                "name": s.name or "",
                "parent_box_id": str(s.parent_box_id) if s.parent_box_id is not None else None,
            }
        sh = state_home()
        if sh.exists():
            for p in sh.glob("*.sqlar"):
                k = p.stem
                if not BOX_ID_RE.match(k): continue
                if k not in meta:
                    nm = sqlar_meta_get(p, "name") or ""
                    pbid = sqlar_meta_get(p, "parent_box_id") or None
                    meta[k] = {"name": nm, "parent_box_id": pbid}
        return meta

    def _name_index(self) -> dict:
        """{name: box_id} over every box (live sessions + on-disk sqlar 'name' meta).
        The reverse of the derived-display-path direction; used by resolve_box."""
        out: dict = {}
        for key, m in self._all_box_meta().items():
            nm = m["name"]
            if nm and nm not in out:
                try: out[nm] = int(key)
                except (ValueError, TypeError): pass
        return out

    def _find_named_child(self, name: str, parent_box_id) -> "str | None":
        """The box key str(box_id) of the box NAMED `name` whose parent is
        `parent_box_id` (None=top-level), else None. Siblings must have unique NAMEs;
        this is the enforcement/rerun lookup. Scans live sessions + on-disk meta."""
        pbid = int(parent_box_id) if parent_box_id is not None else None
        for k, m in self._all_box_meta().items():
            if m["name"] == name:
                v = m["parent_box_id"]
                try: kpbid = int(v) if v else None
                except (ValueError, TypeError): kpbid = None
                if kpbid == pbid:
                    return k
        return None

    def _box_name(self, box_id) -> "str | None":
        """A box's NAME label: the live Session's, else the on-disk sqlar 'name' meta."""
        s = self.sessions.get(str(box_id))
        if s is not None and s.name:
            return s.name
        return sqlar_meta_get(sqlar_path(box_id), "name")

    def _box_parent(self, box_id) -> "int | None":
        """A box's parent box_id: the live Session's, else the on-disk meta. None=top."""
        s = self.sessions.get(str(box_id))
        if s is not None:
            return s.parent_box_id
        v = sqlar_meta_get(sqlar_path(box_id), "parent_box_id")
        try: return int(v) if v else None
        except (ValueError, TypeError): return None

    def display_path(self, box_id) -> str:
        """The dotted DISPLAY path for a box: walk parent_box_id to the root, joining
        each hop's NAME. Presentation only — never a storage key. Falls back to the
        box_id string if a hop has no name (e.g. mid-teardown)."""
        parts = []
        bid = int(box_id)
        seen = set()
        for _ in range(64):
            if bid in seen: break
            seen.add(bid)
            nm = self._box_name(bid)
            parts.append(nm if nm else str(bid))
            p = self._box_parent(bid)
            if p is None: break
            bid = int(p)
        return ".".join(reversed(parts))

    def session_dicts(self) -> list:
        """Every session as a plain dict (to_dict + display 'path') — the UI's
        startup snapshot, JSON-safe so the remote UI gets the same thing over
        the wire that the embedded UI builds locally."""
        out = []
        for s in self.sessions.values():
            d = s.to_dict()
            d["path"] = self.display_path(s.session_id)
            out.append(d)
        return out

    def open_files(self, sid) -> list:
        """The box's currently-OPEN captured-write files [(rel, size), …] — the
        UI's live 'open files' view, delegated to the overlay ops (verb-shaped
        so the remote UI can ask for it without touching the mount object)."""
        if self.mount is None or self.mount.ops is None: return []
        return self.mount.ops.open_files(sid)

    def _refresh_box_paths(self) -> None:
        """Push every LIVE box's current display name into the overlay's session dicts,
        so the FUSE serve thread can gate file rules by box name with a plain dict read.
        Recomputes ALL live boxes (there are few) so an ancestor rename or a reparent
        updates each descendant's cached path too. Call after register/rename/dissolve."""
        if self.mount is None: return
        for sid, s in list(self.sessions.items()):
            if not _box_running(s): continue
            try: self.mount.set_box_path(sid, self.display_path(s.box_id))
            except Exception: pass

    def resolve_box(self, name_or_path_or_id) -> "str | None":
        """Resolve a user-facing identifier to the internal box key str(box_id), or None
        if no such box. Accepts: a plain box_id (decimal); a single NAME; or a dotted
        DISPLAY path (A.B.C) matched against derived display paths. This is the ONE
        name↔box_id boundary — internals always pass str(box_id)."""
        if name_or_path_or_id is None:
            return None
        s = str(name_or_path_or_id)
        # Plain box_id key.
        if BOX_ID_RE.match(s):
            if s in self.sessions or sqlar_path(s).exists() or live_dir(s).exists():
                return s
            return None
        # Single NAME or dotted display path.
        if not valid_dotted_name(s):
            return None
        if "." not in s:
            bid = self._name_index().get(s)
            return str(bid) if bid is not None else None
        # Dotted path: build the display-path lookup once from the snapshot,
        # deriving each box's dotted path from in-snapshot name/parent pointers
        # (no per-ancestor DB reopen).
        meta = self._all_box_meta()
        def _snap_display_path(key: str, snap: dict, depth: int = 64) -> str:
            parts = []
            k = key
            seen: set = set()
            for _ in range(depth):
                if k in seen: break
                seen.add(k)
                m = snap.get(k)
                if m is None: parts.append(k); break
                nm = m["name"]
                parts.append(nm if nm else k)
                pbid = m["parent_box_id"]
                if not pbid: break
                k = str(pbid)
            return ".".join(reversed(parts))
        for k in meta:
            if _snap_display_path(k, meta) == s:
                return k
        return None

    def attach_loop(self, loop) -> None:
        """Called by the ChannelServer once its asyncio loop is up; gives the box
        EchoStreams the loop they frame captured output onto."""
        self._echo_loop = loop

    def _start_echo(self, sid: str) -> "EchoStream | None":
        """Create + start this box's socketless EchoStream (the drain task framing
        captured bytes as ECHO frames). None if no asyncio loop is attached (offline
        tests), in which case capture still records to the table — only live replay is
        absent. The writer is wired later by the ChannelServer once the box's muxed
        connection is up (attach_box_channel)."""
        if self._echo_loop is None:
            return None
        self._stop_echo(sid)
        es = EchoStream(sid, self._echo_loop)
        es.start()
        self._echoes[sid] = es
        return es

    def _stop_echo(self, sid: str) -> None:
        es = self._echoes.pop(sid, None)
        if es is not None:
            try: es.stop()
            except Exception: pass

    def attach_box_channel(self, sid: str, writer) -> None:
        """Wire the box's muxed-connection StreamWriter into its EchoStream so captured
        bytes flow back as ECHO frames. No-op if the box has no EchoStream (not capturing,
        or no loop attached)."""
        es = self._echoes.get(sid)
        if es is not None:
            es.set_writer(writer)

    def detach_box_channel(self, sid: str) -> None:
        """The box's muxed connection is closing: stop using its writer for echo."""
        es = self._echoes.get(sid)
        if es is not None:
            es.set_writer(None)

    def stop_all_echoes(self) -> None:
        for sid in list(self._echoes):
            self._stop_echo(sid)

    def _emit(self, **ev: Any) -> None:
        # Decorate any session dict with its DERIVED dotted display path (presentation
        # only — never a storage key). Computed here at the one emit boundary so every
        # observer (UI, tests) sees the same name↔path rendering.
        s = ev.get("session")
        if isinstance(s, dict) and "path" not in s:
            try: s["path"] = self.display_path(s["session_id"])
            except Exception: s["path"] = s.get("name") or s.get("session_id", "")
        self.on_event(ev)

    async def sweep_orphans(self, loop=None) -> None:
        """At UI startup, consolidate-or-leave each orphaned live/<sid> from a prior
        crash: its runner is gone, so fold its overlay into the single <sid>.sqlar
        (deletions and opacity come from that sqlar) and remove the spent backing
        tree. Leaves
        live/ empty at rest for any session that had no reviewable changes. Each fold
        is the slow deflate pass; run it OFF the loop (executor, awaited) so the UI
        comes up responsive instead of frozen behind a big orphan."""
        sweep_orphan_pools()
        lh = live_home()
        if not lh.exists(): return
        loop = loop or asyncio.get_running_loop()
        for d in sorted(lh.iterdir()):
            if not (d / "up").is_dir(): continue
            sid = d.name
            if not BOX_ID_RE.match(sid): continue           # not a box_id-named backing dir
            # A live backing dir at UI startup is an orphan from a prior run (the box_id
            # carries no pid, so liveness can't be read from it; this UI owns no runner
            # for it). A box launched this run is created after sweep, so never seen here.
            try:
                # No live Index for an orphan: consolidate opens its own sqlar conn over
                # this box's own files, so running it in a thread is safe.
                await loop.run_in_executor(
                    None, consolidate, str(d), sid)
            except Exception as e:
                print(f"slopbox(ui): sweep of {sid} failed: {e}", file=sys.stderr)
                continue
            try:
                if not upper_has_changes(d / "up"):
                    rm_rf(d)
            except OSError:
                pass

    async def consolidate_in_executor(self, loop, sid) -> None:
        """Off-loop consolidation of one FINISHED box for a loop-side awaiter (the
        `patch` control op). Prep + finish run on the loop (registry state); the heavy
        deflate pass runs in the executor. No-op if `sid` needs no consolidation. Safe
        off-loop: a finished box has no live Index — consolidate opens its own sqlar
        conn over that box's own files."""
        args = self.review._consolidate_prep(sid)
        if args is None: return
        # Placement only — apply/discard happen on delete / explicit UI, not here.
        try:
            await loop.run_in_executor(
                None, functools.partial(consolidate, args["shm_dir"], sid))
        finally:
            self.review._consolidate_finish(sid)

    def rescan(self) -> None:
        disk = discover_sessions()
        for sid, s in disk.items():
            cur = self.sessions.get(sid)
            if cur is None:
                self.sessions[sid] = s
                self._emit(type="session_added", session=s.to_dict())
            elif not cur.live:
                # Status is derived (a non-live box can't be running — we don't revive it
                # from a possibly-wrapped pid); only the reviewable-content flag can shift.
                changed = (cur.has_sqlar != s.has_sqlar)
                cur.has_sqlar = s.has_sqlar
                if changed:
                    self._emit(type="session_updated", session=cur.to_dict())
        for sid in list(self.sessions):
            s = self.sessions[sid]
            if s.live: continue
            shm_gone = not (s.shm_dir and Path(s.shm_dir).exists())
            sqlar_gone = not sqlar_path(sid).exists()
            if shm_gone and sqlar_gone:
                del self.sessions[sid]
                self._emit(type="session_removed", session_id=sid)

    def register(self, msg: dict) -> dict:
        """Set up a box's overlay and expose it as a mount subfolder. Returns the ack
        dict the ChannelServer sends back to the runner: the absolute <mnt>/<box_id>
        bind target, so the runner can bwrap onto it. Fail-closed: any setup error is
        reported in the ack, never swallowed.

        Identity model: the box's key is its eagerly-minted integer box_id (str(box_id)
        as a path component). The user-facing NAME is a mutable meta label; nesting is a
        parent_box_id pointer. The runner-supplied session_id is treated only as a
        candidate NAME — never as a path component."""
        if self.mount is None or not self.mount.is_healthy():
            return dict(ok=False, error="overlay mount is not available")
        cmd = msg.get("cmd", [])
        relname = msg.get("relname")
        want_name = msg.get("session_id")   # runner-supplied NAME candidate (or None)
        _derived_parent_sid = msg.get("_derived_parent_sid")  # parent box key; None=top

        # ── PARENT + NAME RESOLUTION ───────────────────────────────────────────
        # IN-BOX (relname present): parent = kernel-derived box key (authoritative); the
        #   box supplies only a single-segment relative NAME (or "" → auto A<n>).
        # HOST (no relname): top-level by default. A supplied NAME may be a single
        #   segment (top-level) or a dotted display path whose prefix names the parent.
        parent_box_id: "int | None" = None
        name: "str | None" = None

        if relname is not None:
            if relname and (not valid_name(relname) or "." in relname or "/" in relname):
                return dict(ok=False,
                            error="invalid relname: must be a single NAME segment "
                                  "(no dots, no slashes, no '..')")
            if not _derived_parent_sid:
                return dict(ok=False, error="relname supplied but no enclosing box found")
            enclosing = self.sessions.get(str(_derived_parent_sid))
            if enclosing is None or not enclosing.live:
                return dict(ok=False,
                            error=f"enclosing box {_derived_parent_sid!r} is not live")
            parent_box_id = enclosing.box_id
            name = relname or None
        else:
            # Host launch. Optional kernel-derived parent (a host runner that is itself
            # inside a box, unusual) — honour it like the in-box path.
            if _derived_parent_sid:
                ps = self.sessions.get(str(_derived_parent_sid))
                if ps is None or not ps.live:
                    return dict(ok=False,
                                error=f"derived parent {_derived_parent_sid!r} is not live")
                parent_box_id = ps.box_id
            if want_name:
                if not valid_dotted_name(want_name):
                    return dict(ok=False, error="invalid name")
                if "." in want_name:
                    # Dotted display path: parent = prefix, NAME = last segment. The
                    # prefix must resolve to an existing box (fail-closed).
                    prefix, _, last = want_name.rpartition(".")
                    pk = self.resolve_box(prefix)
                    if pk is None:
                        return dict(ok=False,
                                    error=f"parent box '{prefix}' does not exist "
                                          f"(create it before launching a child)")
                    parent_box_id = int(pk)
                    name = last
                else:
                    name = want_name

        # ── CREATE-VS-RERUN ────────────────────────────────────────────────────
        # A named box reruns the SAME box_id if a sibling with that NAME already exists
        # under the resolved parent. An unnamed launch always CREATEs (fresh box_id).
        sid: "str | None" = None
        rerun = False
        if name:
            existing = self._find_named_child(name, parent_box_id)
            if existing is not None:
                sid = existing
                rerun = True
        if not rerun:
            # Fresh box: mint its id eagerly so a path is always derivable from it.
            name = name or self._next_auto_name()
            # Enforce unique NAMEs among siblings (only at create — see rename()).
            if self._find_named_child(name, parent_box_id) is not None:
                return dict(ok=False,
                            error=f"name '{name}' already in use under this parent")
            sid = str(mint_box_id())

        if rerun and (sid in self.indexes
                      or (self.sessions.get(sid) and self.sessions[sid].live)):
            return dict(ok=False, error="slopbox is already running")

        backing = live_dir(sid)
        up = backing / "up"
        try:
            # 0700: live/<box_id> holds provenance.
            old = os.umask(0o077)
            try:
                backing.mkdir(parents=True, exist_ok=True)
                up.mkdir(parents=True, exist_ok=True)
            finally:
                os.umask(old)
            os.chmod(backing, 0o700)
            if rerun and sqlar_path(sid).exists():
                unconsolidate(backing)             # ensure up/ sentinel exists
            index = Index(backing)
        except (OSError, sqlite3.Error) as e:
            return dict(ok=False, error=f"overlay setup failed: {e}")
        env_capture = bool(msg.get("want_env"))
        direct = bool(msg.get("want_direct"))
        want_capture = bool(msg.get("want_capture"))
        index.set_env_capture(env_capture)
        # The box's ROOT process row: tgid == the runner's host pid, argv == the workload
        # command. Prefer the pidfd-derived HOST pid; the runner's self-reported
        # os.getpid() is box-namespaced when the runner is itself inside a box.
        root_prov = dict(msg.get("prov") or {})
        host_root = (int(msg.get("_register_host_pid") or 0)
                     or int(msg.get("root_tgid") or 0))
        root_prov["tgid"] = host_root
        # start_time identifies the incarnation; the runner's self-reported start (from
        # its own /proc) matches the common case, but if the host pid differs (nested
        # box) read the host pid's start so the (tgid,start) identity is host-correct.
        hs = _proc_start_time(host_root)
        root_prov["start_time"] = hs or int(root_prov.get("start_time") or 0)
        root_prov["argv"] = list(cmd)
        index.process_from_prov(root_prov, root=True)
        self.indexes[sid] = index
        self.mount.add_session(sid, up, index, passthrough=direct,
                               parent=str(parent_box_id) if parent_box_id else None)
        # Persist the NAME label + parent pointer in meta so a box discovered after a UI
        # restart keeps its name and nesting.
        try: sqlar_meta_set(sqlar_path(sid), "name", name)
        except Exception: pass
        if parent_box_id is not None:
            try: sqlar_meta_set(sqlar_path(sid), "parent_box_id", str(parent_box_id))
            except Exception: pass
        box_root = str(mnt_point() / sid)
        # Close any held pidfd from a prior run of this sid to avoid a leak.
        _old = self.sessions.get(sid)
        if _old is not None and _old.run_pidfd >= 0:
            try: os.close(_old.run_pidfd)
            except OSError: pass
        s = Session(session_id=sid, box_id=int(sid), name=name, cmd=cmd,
                    shm_dir=str(backing), live=True, run_pid=root_prov["tgid"],
                    parent_box_id=parent_box_id)   # kernel/name-derived; None for top-level
        # Dup the runner's pidfd into the session so we own a persistent liveness handle.
        # _handle keeps its own copy and closes it after this call; we need a separate fd.
        _rfd = int(msg.get("_register_pidfd", -1))
        if _rfd >= 0:
            try: s.run_pidfd = os.dup(_rfd)
            except OSError: s.run_pidfd = -1
        self.sessions[sid] = s
        # Now that the box's name + parent are recorded, cache its (and any descendant's)
        # display name into the overlay so file rules can be box-scoped from first write.
        self._refresh_box_paths()
        self.review._consolidated.discard(sid)
        # HIGH-1: mint an owner token kept OUTSIDE the box (returned to the runner
        # only). The CLI drop op presents it; a box tears itself down by closing its own
        # muxed connection (which carries the same token), so no box can tear down
        # ANOTHER session — it has only its own connection.
        token = uuid.uuid4().hex
        self._owner_tokens[sid] = token
        # Capture mode (default; suppressed by -t): expose the two hidden sink files at
        # the box root so the child's stdout/stderr write THROUGH the FUSE layer (giving
        # us per-write ctx.pid attribution), and start the EchoStream that frames the
        # captured bytes as ECHO frames on the box's muxed connection (writer wired by the
        # ChannelServer once that connection is serving — attach_box_channel).
        echo = None
        if want_capture and self.mount is not None and self.mount.ops is not None:
            self.mount.ops.add_sink(sid, SINK_STDOUT_REL, 0)
            self.mount.ops.add_sink(sid, SINK_STDERR_REL, 1)
            echo = self._start_echo(sid)
            if echo is not None:
                self.mount.ops.set_echo_queue(sid, echo.enqueue, echo.flush_and_close)
        self._emit(type="session_added", session=s.to_dict())
        # ONE socket: the runner already holds the connection it registered on; that
        # SAME connection becomes the box's muxed channel (ECHO/MUTE). No echo socket
        # path is advertised — there is none. NESTED LAUNCH: a nested runner roots
        # bwrap by binding the parent-exposed /<KIDS_DIR>/<sid> overlay root; the
        # bind-mounted ui.sock at UI_SOCK_INBOX carries its channel.
        ack = dict(ok=True, mount=box_root, shm_dir=str(backing),
                   owner_token=token, box_id=int(sid), session_id=sid, name=name,
                   capture=want_capture)
        return ack

    def _owns(self, msg: dict, sid: str) -> bool:
        """A teardown control op is honoured only if it presents the session's owner
        token. The token never enters the box, so in-box code cannot forge it."""
        want = self._owner_tokens.get(sid)
        if want is None:
            # No token on record (e.g. a session discovered from disk at startup, never
            # registered this run). Such sessions were never live this process, so any
            # teardown is a local bookkeeping op, not a cross-session attack surface.
            return True
        return msg.get("owner_token") == want

    def unregister(self, msg: dict) -> None:
        """Synchronous teardown: the fold runs inline on the CALLING thread. Used by
        non-UI callers (control-plane tests, an echo-loop caller) where blocking that
        thread is fine. The interactive UI must NOT use this — `_dispatch_control`
        calls unregister_async, which folds OFF the loop (awaited) so the UI never
        freezes. Both share the same prep/fold/post helpers, so the resulting sqlar is
        identical; only WHERE the fold runs differs."""
        ctx = self._teardown_prep(msg)
        if ctx is None: return
        sid, s, index, backing, reported_status = ctx
        self._teardown_fold(sid, s, index, backing, reported_status, executor=None)
        self._teardown_post(msg, sid, s, index, backing, reported_status)

    async def unregister_async(self, msg: dict, loop) -> None:
        """Loop-side teardown: identical to unregister() but the slow deflate fold runs
        in the executor, AWAITED — the UI loop stays free during the minute-long fold
        while the box is fully consolidated by the time this returns, so programmatic
        observers (test_e2e / test_control_plane / `slopbox patch`) that read results
        right after a box reports finished see the consolidated result with no race."""
        ctx = self._teardown_prep(msg)
        if ctx is None: return
        sid, s, index, backing, reported_status = ctx
        aw = self._teardown_fold(sid, s, index, backing, reported_status,
                                 executor=loop)
        if aw is not None:
            await aw
        self._teardown_post(msg, sid, s, index, backing, reported_status)

    def _teardown_prep(self, msg: dict):
        """Loop/thread-side teardown setup, shared by sync + async unregister: validate,
        check the owner token, stop the echo stream, then remove the box from the live
        mount set (this DETACHES it from the FUSE serve thread, so its live Index becomes
        QUIESCENT — the single writer is done with it) and pop the Index handle. Returns
        (sid, s, index, backing, reported_status) or None to abort."""
        sid = msg.get("session_id")
        if not valid_box_id(sid):
            return None
        if not self._owns(msg, sid):
            print(f"slopbox(ui): rejected unregister of {sid} (bad owner token)",
                  file=sys.stderr)
            return None
        self._stop_echo(sid)
        self._owner_tokens.pop(sid, None)
        s = self.sessions.get(sid)
        reported_status = msg.get("status", "finished")
        if self.mount is not None:
            self.mount.remove_session(sid)
        index = self.indexes.pop(sid, None)
        return sid, s, index, live_dir(sid), reported_status

    def _teardown_fold(self, sid, s, index, backing, reported_status, executor):
        """Consolidate the box's overlay into its single sqlar (fill NULL blobs + record
        provenance). If `executor` (the event loop) is given, run the fold there and
        return an awaitable; otherwise fold inline and return None. The Index is passed
        through: remove_session() in _teardown_prep already detached it from the serve
        thread, so it is quiescent (consolidate only READS deletions()/opaque_dirs() —
        GIL-atomic — and WRITES the sqlar over its own conn). It is close()d AFTER the
        fold (in _teardown_post). Show the placeholder + #cons-bar via _consolidating
        (loop-confined, set/cleared here) and progress events (post_message is thread-
        safe, so an executor-thread progress callback marshals fine). Fail-closed on a
        consolidate error: log and continue teardown. If bwrap never launched
        (status="error"), skip the fold — the upper holds nothing the box wrote.

        Returns an awaitable when `executor` is set, else None — unregister_async awaits
        it, the sync path ignores it."""
        if not (backing.exists() and reported_status != "error"):
            return None
        self.review._consolidating.add(sid)
        self._emit(type="consolidate_started", session_id=sid)
        def _prog(done, total):
            self._emit(type="consolidate_progress", session_id=sid,
                       done=done, total=total)
        def _done(err):
            if err is not None:
                print(f"slopbox(ui): consolidate {sid} failed: {err}", file=sys.stderr)
            self.review._consolidating.discard(sid)
            self._emit(type="consolidate_finished", session_id=sid)
        if executor is None:
            try:
                consolidate(str(backing), sid, index=index, progress=_prog)
                _done(None)
            except Exception as e:
                _done(e)
            return None
        async def _run():
            try:
                await executor.run_in_executor(
                    None, functools.partial(consolidate, str(backing), sid,
                                            index=index, progress=_prog))
                _done(None)
            except Exception as e:
                _done(e)
        return _run()

    def _teardown_post(self, msg, sid, s, index, backing, reported_status):
        """Loop/thread-side teardown finish, shared by sync + async unregister: close the
        Index AFTER the fold, drop the spent backing tree, then update session
        status/has_sqlar. live/ + the synthetic root are empty at rest."""
        if index is not None:
            index.close()
        try:
            if not (backing.exists() and upper_has_changes(backing / "up")):
                rm_rf(backing)
        except OSError:
            pass
        if not s:
            return
        s.live = False
        # Release the runner's liveness pidfd now that the session is done.
        if s.run_pidfd >= 0:
            try: os.close(s.run_pidfd)
            except OSError: pass
            s.run_pidfd = -1
        # live=False + pidfd released ⇒ .status now derives to stopped; record WHY.
        # (a killed box already carries s.killed; "finished" needs no flag.)
        if reported_status == "error":
            s.errored = True
        elif reported_status == "killed":
            s.killed = True
        s.exit_code = msg.get("exit_code")
        s.has_sqlar = sqlar_nonempty(sqlar_path(sid))
        self.review._consolidated.add(sid)
        # nothing reviewable at all -> drop the entry entirely
        if not (s.has_sqlar
                or (backing.exists() and upper_has_changes(backing / "up"))):
            self.sessions.pop(sid, None)
            self._emit(type="session_removed", session_id=sid)
        else:
            s.shm_dir = str(backing)
            self._emit(type="session_updated", session=s.to_dict())

    def drop(self, msg: dict) -> None:
        # Runner reports the session left nothing to review; remove it outright.
        sid = msg.get("session_id")
        if not valid_box_id(sid):
            return
        if not self._owns(msg, sid):
            print(f"slopbox(ui): rejected remove of {sid} (bad owner token)",
                  file=sys.stderr)
            return
        self._stop_echo(sid)
        self._owner_tokens.pop(sid, None)
        _s = self.sessions.pop(sid, None)
        if _s is not None:
            if _s.run_pidfd >= 0:
                try: os.close(_s.run_pidfd)
                except OSError: pass
                _s.run_pidfd = -1
            self._emit(type="session_removed", session_id=sid)

    def _box_exists(self, sid) -> bool:
        """True if `sid` (a box key str(box_id)) names an existing box. Callers that
        accept a user-facing name resolve it via resolve_box FIRST."""
        return bool(sid and (sid in self.sessions or sqlar_path(sid).exists()
                             or live_dir(sid).exists()))

    def select(self, ident) -> dict:
        """Resolve a user identifier (box_id, NAME, or dotted display path) to its box
        key and make it the UI's selected box (the target of bare `slopbox patch`/
        `apply`/`discard`/`rename`). Error if no such box."""
        sid = self.resolve_box(ident)
        if sid is None:
            return dict(ok=False, error=f"no slopbox '{ident}'")
        self.selected_sid = sid
        self._emit(type="select_box", session_id=sid)   # move the UI cursor too
        return dict(ok=True, sid=sid)

    def rename(self, ident, new_name) -> dict:
        """Rename a box: write its NAME meta label and emit an update. NO file move, NO
        id change — the box's identity is its box_id, which never changes. Works on a
        live box. `new_name` must be a single NAME segment, unique among the box's
        siblings (same parent)."""
        sid = self.resolve_box(ident)
        if sid is None:
            return dict(ok=False, error="no slopbox selected")
        # The NAME label is a single segment (the dotted form is a derived display path,
        # never stored). Reject dotted/invalid names.
        if not valid_name(new_name):
            return dict(ok=False, error="invalid name: a single CAPS segment "
                        "(e.g. BOB), no dots, no trailing '-'")
        s = self.sessions.get(sid)
        old = (s.name if s else None) or self._box_name(sid) or sid
        if new_name == old:
            return dict(ok=True, sid=sid, old=old, name=new_name)
        # Sibling NAMEs must be unique under the same parent.
        clash = self._find_named_child(new_name, self._box_parent(sid))
        if clash is not None and clash != sid:
            return dict(ok=False, error=f"name '{new_name}' is already in use "
                        "under this parent")
        try:
            sqlar_meta_set(sqlar_path(sid), "name", new_name)
        except sqlite3.Error as e:
            return dict(ok=False, error=f"rename failed: {e}")
        if s is not None:
            s.name = new_name
            self._emit(type="session_updated", session=s.to_dict())
        else:
            # Finished/discovered box with no live Session: surface the change anyway.
            self._emit(type="session_renamed", session_id=sid, name=new_name)
        # The new name changes this box's display path and every live descendant's.
        self._refresh_box_paths()
        return dict(ok=True, sid=sid, old=old, name=new_name)

    def outputs(self, sid) -> list:
        return outputs_list(sqlar_path(sid))

    def output_detail(self, sid, oid) -> "dict | None":
        return outputs_get(sqlar_path(sid), oid)

    def processes(self, sid) -> list:
        """The box's process table: [(id, tgid, ppid, exe, argv-list)]."""
        return process_list(sqlar_path(sid))

    def processes_live(self, sid) -> "list | None":
        """The live, bounded ACTIVE SET for a running box (htop-style), decorated by
        the live Index from its in-memory caches — O(active set), no full-table query.
        None once the box is no longer running (its runner root is gone, i.e. zero
        running processes) OR has no live Index: the UI then renders the cached full
        `processes(sid)` history instead. Keying the live/finished switch on runner-root
        liveness — the same signal the changes, processes, and outputs panes use — means all three
        live panes flip to the complete record at the same instant the box stops,
        rather than the proc pane briefly showing an empty active set until teardown
        drops the Index."""
        s = self.sessions.get(sid)
        if s is not None and not _box_running(s):
            return None                      # zero running processes: show full history
        idx = self.indexes.get(sid)
        if idx is None: return None
        return idx.live_processes()

    def proc_info(self, sid, row_id) -> "tuple | None":
        """(tgid, ppid, parent_id, exe, argv) for any process ROW id the live box's Index
        ever recorded, so the tree builder can resolve connector ancestors outside the
        active set. None for a finished box (its cached full table is self-contained)."""
        idx = self.indexes.get(sid)
        return idx.proc_info(row_id) if idx is not None else None

    def proc_roots(self, sid) -> set:
        """The box's hierarchy-root ROW ids (the proc-tree walk boundary). Live box → its
        in-RAM Index; finished/discovered → the sqlar's process.root column."""
        idx = self.indexes.get(sid)
        if idx is not None: return idx.roots()
        return process_roots(sqlar_path(sid))

    def process_env(self, sid, proc_id) -> dict:
        return process_env(sqlar_path(sid), proc_id)

    def writer_id(self, sid, rel) -> "int | None":
        """The process-table row id that wrote file `rel` in box `sid` (its proc-pane
        key), for cross-pane jumps. Live box → its in-RAM Index; finished → the sqlar."""
        idx = self.indexes.get(sid)
        if idx is not None: return idx.writer_id_for(rel.lstrip("/"))
        return sqlar_writer_id(sqlar_path(sid), rel.lstrip("/"))

    def first_writer_id(self, sid, rel) -> "int | None":
        """The row id of the FIRST process to write `rel` in box `sid` (the writer a file
        rule's process facets lock to). Live box → its in-RAM Index; finished → sqlar."""
        idx = self.indexes.get(sid)
        if idx is not None: return idx.first_writer_id_for(rel.lstrip("/"))
        return sqlar_first_writer_id(sqlar_path(sid), rel.lstrip("/"))

    def first_writer_prov(self, sid, rel) -> "dict | None":
        """Provenance (exe/cwd/argv) of the path's FIRST writer — the Subject a change's
        filter facets match (consistent with file rules). Live Index → finished sqlar."""
        idx = self.indexes.get(sid)
        if idx is not None: return idx.first_writer_provenance(rel.lstrip("/"))
        return sqlar_first_writer_prov(sqlar_path(sid), rel.lstrip("/"))

    def proc_prov(self, sid, row_id) -> "dict | None":
        """Provenance (exe/cwd/argv) of one process ROW — the procs-pane filter Subject.
        Live box → its in-RAM Index cache; finished → the sqlar process table."""
        idx = self.indexes.get(sid)
        if idx is not None: return idx.proc_prov_for(row_id)
        return sqlar_proc_prov(sqlar_path(sid), row_id)

    def kill(self, ident) -> None:
        # s.pid is the runner ("slopbox CMD") process. SIGTERM to it trips the runner's
        # handler, which tears down the bwrap group and unmounts; we must NOT killpg here
        # — the runner shares the launching shell's process group.
        sid = self.resolve_box(ident)
        s = self.sessions.get(sid) if sid is not None else None
        if not s or not _box_running(s): return
        try: os.kill(s.pid, signal.SIGTERM)
        except ProcessLookupError: pass
        # Record intent; .status stays "running" until the runner root actually dies and
        # teardown releases the pidfd, at which point it derives to "killed".
        s.killed = True
        self._emit(type="session_updated", session=s.to_dict())

    def _free_storage(self, sid) -> None:
        """Low-level, NON-finalizing teardown: detach from the live mount, close+drop
        the live Index, and remove the box's backing tree + ALL on-disk stores (sqlar,
        WAL/shm, and the permanent pool dir). Used by dissolve() once a box's changes are
        finalized/spliced, and as the forced free for an already-empty box. Idempotent;
        does nothing for a still-running box. Handles both an in-sessions box and an
        sqlar-only box (no Session object)."""
        s = self.sessions.get(sid)
        if s is not None and _box_running(s):
            return                                  # refuse to free a running box
        if self.mount is not None:
            self.mount.remove_session(sid)
        idx = self.indexes.pop(sid, None)
        if idx is not None:
            try: idx.close()
            except Exception: pass
        if s is not None and s.shm_dir:
            rm_rf(s.shm_dir)
        sp = sqlar_path(sid)
        # Remove the box's permanent pool files too (its sqlar may hold resident rows
        # whose bytes live only in blob/<box_id>/ — validated before use).
        bid = sid if BOX_ID_RE.match(str(sid)) else None
        _sqlar_unregister(sp)   # close + drop any cached connection before unlinking the db
        for suf in ("", "-wal", "-shm"):
            p = Path(str(sp) + suf)
            if p.exists():
                try: p.unlink()
                except OSError: pass
        if bid is not None:
            try: rm_rf(box_pool_dir(int(bid)))
            except (OSError, ValueError, TypeError): pass
        if s is not None and s.run_pidfd >= 0:
            try: os.close(s.run_pidfd)
            except OSError: pass
            s.run_pidfd = -1
        self.sessions.pop(sid, None)
        self._emit(type="session_removed", session_id=sid)

    def delete(self, ident, finalize: bool = False) -> "dict | None":
        """Remove a box. finalize=True routes to dissolve() (finalize the box's changes
        via the file rules — apply-matched promoted up/host, the rest discarded with
        copy-down — then re-parent its direct children and free its storage,
        fail-closed). finalize=False is a forced, NON-finalizing free of storage for a
        box that already holds nothing the caller wants kept (e.g. the deferred reap of
        an emptied box, or after an explicit apply/discard). Returns dissolve()'s dict
        when finalizing, else None."""
        if finalize:
            return self.dissolve(ident)
        sid = self.resolve_box(ident)
        if sid is None:
            return None
        s = self.sessions.get(sid)
        if s is None and not sqlar_path(sid).exists():
            return None
        if s is not None and _box_running(s):
            return None
        self._free_storage(sid)
        return None

    def dissolve(self, ident) -> dict:
        """The ONE box-removal operation. Finalize sid's changes (apply-matched files
        promoted up/host; the rest discarded — each copied DOWN into the immediate
        children that inherit it, so no descendant's merged view changes), then RE-PARENT
        each direct child by pointing its parent_box_id at sid's own parent_box_id (a
        plain pointer write — valid while a child is live), then free sid's storage
        (sqlar + pool). FAIL-CLOSED: if finalizing (or any copy-down) reports errors,
        NOTHING is freed and NO re-parent happens — the error is surfaced and state is
        left intact.

        Precondition: sid exists and is not live/running. (No descendant-liveness
        refusal: re-parenting is now a pointer write, valid while the child is live.)"""
        sid = self.resolve_box(ident)
        if sid is None:
            return dict(ok=False, error=f"no slopbox '{ident}'")
        if self.review._live(sid):
            return dict(ok=False, error=f"'{sid}' is running; stop it first")
        # ── finalize sid's OWN changes (FAIL-CLOSED) ──────────────────────────────
        # apply-matched files promote up; the rest are discarded, copying each down into
        # the immediate children that inherit it. If anything errors we must NOT free the
        # storage or re-parent — the whole safety point of finalizing before removal.
        try:
            res = self.review.finalize_by_rules(sid)
        except Exception as e:
            return dict(ok=False, error=f"finalizing '{sid}' failed: {e}")
        if res.get("errors"):
            return dict(ok=False,
                        error=f"finalizing '{sid}' had errors; nothing removed",
                        finalize_errors=res["errors"])
        # ── re-parent each direct child to sid's parent (pointer write) ──────────
        new_parent = self._box_parent(sid)   # int | None (sid's own parent)
        reparented = []
        for child in self.review._immediate_children(sid):
            try:
                if new_parent is None:
                    # Child becomes top-level: drop the meta pointer.
                    sqlar_meta_set(sqlar_path(child), "parent_box_id", "")
                else:
                    sqlar_meta_set(sqlar_path(child), "parent_box_id", str(new_parent))
            except Exception:
                pass
            cs = self.sessions.get(child)
            if cs is not None:
                cs.parent_box_id = new_parent
            if self.mount is not None and hasattr(self.mount, "set_parent"):
                self.mount.set_parent(
                    child, str(new_parent) if new_parent is not None else None)
            reparented.append(child)
        # Re-parenting changed each live child's (and its descendants') display path.
        self._refresh_box_paths()
        # ── free sid's storage ────────────────────────────────────────────────────
        self._free_storage(sid)
        return dict(ok=True, deleted=sid, reparented=reparented)

    def _box_is_empty(self, sid) -> bool:
        """True when a finished box holds nothing at all — no overlay change and no
        sqlar entry. Live/running boxes are never 'empty' for removal purposes. This
        is a pure PREDICATE: it never deletes (so it is safe to call from inside a
        loop)."""
        s = self.sessions.get(sid)
        if not s or _box_running(s): return False
        # Guard: when shm_dir is empty, Path(s.upper) resolves to the relative path
        # "up", which would walk the process cwd — wrong. Treat as no overlay.
        if not s.shm_dir:
            overlay = False
        else:
            try: overlay = upper_has_changes(Path(s.upper))
            except OSError: overlay = False
        sqlar = bool(sqlar_list(sqlar_path(sid)))
        return not overlay and not sqlar

    def _reap_empty(self, sids) -> None:
        """DEFERRED, top-level empty-box removal: after an operation that removes changes
        (apply/discard/finalize/dissolve), delete the boxes it touched that now hold
        nothing. Kept out of the low-level helpers so a box is never deleted out from
        under a loop ('rugpull'). Accepts one sid or an iterable; ignores unknown ones."""
        if isinstance(sids, str): sids = [sids]
        for sid in list(sids or ()):
            if self._box_is_empty(sid):
                self.delete(sid)

FRAME_ECHO = 2

FRAME_ECHO_DONE = 3

FRAME_MUTE = 4

FRAME_UNMUTE = 5

def encode_frame(ftype: int, payload: bytes = b"") -> bytes:
    """Encode one typed frame: [total-len:4 BE][type:1][payload]. total-len counts the
    type byte + payload. Module-level + pure so it is unit-testable without a socket."""
    return struct.pack("!I", 1 + len(payload)) + bytes([ftype & 0xFF]) + payload

def decode_frames(buf: bytes) -> "tuple[list, bytes]":
    """Decode as many whole frames as `buf` holds. Returns (frames, remainder) where
    frames is [(ftype, payload)] and remainder is the trailing partial frame (carry it
    into the next call). Pure: a stream reader feeds bytes in and re-feeds the
    remainder, so a frame split across reads reassembles."""
    out = []
    i = 0
    n = len(buf)
    while n - i >= 4:
        (tot,) = struct.unpack("!I", buf[i:i + 4])
        if n - (i + 4) < tot:
            break                                  # partial frame: stop, keep remainder
        ftype = buf[i + 4]
        payload = buf[i + 5:i + 4 + tot]
        out.append((ftype, payload))
        i += 4 + tot
    return out, buf[i:]

def echo_payload(stream: int, data: bytes) -> bytes:
    """Body of an ECHO frame: [stream:1][bytes]. stream 0=stdout, 1=stderr."""
    return bytes([stream & 0xFF]) + data

_MUTED_HOST_PIDS: set = set()

_ECHO_QUEUE_MAX = 256       # bounded: a slow echo consumer backpressures one FUSE write

class _ConnWriter:
    """A minimal write-only, loop-confined framed writer over a raw non-blocking socket.

    The box's muxed connection is read via recvmsg (FD passing) on the asyncio loop; we
    must NOT layer an asyncio StreamReader/transport over the SAME socket (its reader
    would steal the inbound MUTE/UNMUTE frames). So ECHO/ECHO_DONE frames are written here
    with a plain non-blocking send + add_writer backpressure, all on the loop thread —
    interleaving cleanly with the recvmsg read loop on the same fd (full-duplex)."""
    def __init__(self, conn: socket.socket, loop: asyncio.AbstractEventLoop):
        self._conn = conn; self._loop = loop; self._buf = bytearray()

    def write(self, data: bytes) -> None:
        self._buf += data

    async def drain(self) -> None:
        while self._buf:
            try:
                n = self._conn.send(self._buf)
                del self._buf[:n]
            except (BlockingIOError, InterruptedError):
                fut = self._loop.create_future()
                fd = self._conn.fileno()
                self._loop.add_writer(fd, lambda: fut.done() or fut.set_result(None))
                try:
                    await fut
                finally:
                    try: self._loop.remove_writer(fd)
                    except (OSError, ValueError): pass
            except OSError as e:
                raise ConnectionError(str(e))

    def close(self) -> None:
        self._buf.clear()

class EchoStream:
    """Per-session UI-side echo source — socketless. The sink write() handler calls
    enqueue((stream, bytes)) (awaitable, bounded → backpressure); a drain task frames
    the queued bytes as ECHO frames onto the box's ONE muxed connection (set via
    set_writer once the register handshake on that connection finishes). On close (both
    sinks released at child exit) the queue is flushed and an ECHO_DONE frame is written
    so --inner stops and closes the connection (no truncation).

    Robust if the writer is never attached or drops early: enqueue never blocks forever —
    when no writer is attached we keep at most _ECHO_QUEUE_MAX frames and drop the oldest,
    so the table still records every write and the sink writes never deadlock."""
    def __init__(self, sid: str, loop: asyncio.AbstractEventLoop):
        self.sid = sid; self._loop = loop
        self._drain_task = None
        self._queue: "asyncio.Queue" = asyncio.Queue(maxsize=_ECHO_QUEUE_MAX)
        self._writer: "asyncio.StreamWriter | None" = None
        self._closing = False

    def start(self) -> None:
        self._drain_task = asyncio.create_task(self._drain_loop())

    def set_writer(self, writer: "asyncio.StreamWriter | None") -> None:
        """Attach the box connection's StreamWriter (the muxed channel). ECHO/ECHO_DONE
        frames are written to it; serialized via the same writer the teardown side
        already owns, so the drain task is the only ECHO producer."""
        self._writer = writer

    def enqueue(self, stream: int, payload: bytes,
                block: bool = True) -> "asyncio.Future":
        """Schedule (stream, payload) onto the bounded queue from ANOTHER thread (the
        FUSE serve/trio thread). Returns a concurrent.futures.Future the caller awaits
        (off the asyncio loop) so a full queue applies backpressure to that one write.

        Never deadlocks: when no writer is attached the oldest frame is dropped on a full
        queue (the table already has the bytes). With block=False (used for MUTED readback
        writes — see the FUSE write handler) we ALSO drop-oldest on a full queue even with
        a writer attached, so the put can never block: a muted readback write must never
        wait on this drain, or a descendant box's echo reader and this queue deadlock."""
        async def _put():
            if self._closing:
                return
            if (self._writer is None or not block) and self._queue.full():
                try: self._queue.get_nowait()      # drop oldest, never block
                except asyncio.QueueEmpty: pass
            await self._queue.put((stream, payload))
        return asyncio.run_coroutine_threadsafe(_put(), self._loop)

    async def _drain_loop(self) -> None:
        while True:
            try:
                stream, payload = await self._queue.get()
            except asyncio.CancelledError:
                break
            w = self._writer
            if stream is None:                      # sentinel: flush done, send ECHO_DONE
                if w is not None:
                    try:
                        w.write(encode_frame(FRAME_ECHO_DONE))
                        await w.drain()
                    except (OSError, ConnectionError):
                        self._writer = None
                continue
            if w is None:
                continue                            # no writer yet: bytes already recorded
            try:
                w.write(encode_frame(FRAME_ECHO, echo_payload(stream, payload)))
                await w.drain()
            except (OSError, ConnectionError):
                self._writer = None

    def flush_and_close(self) -> None:
        """Both sinks released (child exited): drain the queue to the box conn, then send
        ECHO_DONE so --inner reads all remaining bytes before closing. Idempotent."""
        if self._closing: return
        self._closing = True
        # A None-stream sentinel ordered AFTER all queued frames triggers the ECHO_DONE
        # once the drain task has flushed everything ahead of it.
        try: asyncio.run_coroutine_threadsafe(
                self._queue.put((None, b"")), self._loop)
        except RuntimeError: pass

    def stop(self) -> None:
        self._closing = True
        if self._drain_task is not None:
            self._drain_task.cancel()
        self._writer = None

def _host_pid_from_pidfd(pidfd: int) -> int:
    """Resolve a pidfd to a HOST-namespace pid by parsing /proc/self/fdinfo/<pidfd>.

    A pidfd's fdinfo `Pid:` line reports the target's pid in the READER's pid
    namespace — and the UI runs in the host namespace, so the value we read IS the
    host pid (this is what makes the box→pidfd→UI chain yield a host-namespace pid
    instead of the box-namespace pid SO_PEERCRED would give). A reaped/dead process
    shows `Pid:\t-1` (or 0); the line may list several space-separated pids across
    nested namespaces, and the first is the one in our namespace. Returns the host
    pid, or 0 on a dead process / unparseable fdinfo."""
    try:
        with open(f"/proc/self/fdinfo/{pidfd}", "rb") as f:
            for line in f:
                if line.startswith(b"Pid:"):
                    rest = line.split(b":", 1)[1].split()
                    return int(rest[0]) if rest else 0
    except (OSError, ValueError, IndexError):
        return 0
    return 0

def _recvmsg_blocking(conn):
    conn.settimeout(30)
    try:
        # Up to 16 fds: --inner sends [request_socket] or [request_socket, pidfd].
        return conn.recvmsg(65536, socket.CMSG_SPACE(4 * 16))
    finally:
        conn.setblocking(False)

def _send_reply(conn: socket.socket, reply: dict) -> None:
    """Send a control reply JSON line (newline-terminated) back to the runner.

    Called via run_in_executor so the async loop stays unblocked while sendall blocks.
    Raises OSError on send failure (caller catches and ignores, as before)."""
    payload = (json.dumps(reply) + "\n").encode()
    conn.setblocking(True)
    try:
        conn.sendall(payload)
    finally:
        conn.setblocking(False)

def _ppid_of(pid: int) -> int:
    """PPid of pid from /proc/<pid>/status; 0 if unreadable."""
    v = _proc_status_field(pid, "PPid")
    return v if v is not None else 0

def _derive_parent_sid(peer_pid: int, sup: "Supervisor") -> "str | None":
    """Given the host-namespace pid of the connecting runner, walk the /proc PPid
    chain upward and return the sid of the first live session whose run_pid matches
    an ancestor of peer_pid — gated by the session's held pidfd so the match is
    wrap-immune (the pidfd names one exact process incarnation; a finished runner
    can never alias a reused pid).

    Security: peer_pid is derived from the runner's pidfd via _host_pid_from_pidfd
    (reads /proc/self/fdinfo/<pidfd> in the UI's host namespace) — the same path
    every other host-pid derivation in the codebase uses.  It is never taken from
    SO_PEERCRED or from the message body; only this kernel-trusted ancestry walk
    is used.  The sid is never trusted from the box."""
    if peer_pid <= 1:
        return None
    # Build {run_pid -> sid} over sessions whose runner pidfd is STILL alive.
    # _pidfd_alive is the sole liveness gate — no start_time, no kill(0).
    root_map: dict = {}
    for sid, sess in sup.sessions.items():
        if sess.live and _pidfd_alive(sess.run_pidfd) and sess.run_pid > 0:
            root_map[sess.run_pid] = sid
    if not root_map:
        return None
    # Walk the PPid chain from peer_pid upward; first match wins.
    seen: set = set()
    pid = tgid_of(peer_pid)
    for _ in range(64):
        if pid <= 1:
            break
        if pid in seen:
            break
        seen.add(pid)
        if pid in root_map:
            return root_map[pid]
        ppid = _ppid_of(pid)
        if ppid <= 1:
            break
        pid = tgid_of(ppid)
    return None

def wire_encode(o):
    if isinstance(o, (bytes, bytearray, memoryview)):
        return {"__b": base64.b64encode(bytes(o)).decode()}
    if isinstance(o, (list, tuple)):
        return [wire_encode(x) for x in o]
    if isinstance(o, (set, frozenset)):
        return {"__s": [wire_encode(x) for x in sorted(o, key=repr)]}
    if isinstance(o, dict):
        if all(isinstance(k, str) for k in o):
            return {k: wire_encode(v) for k, v in o.items()}
        return {"__d": [[wire_encode(k), wire_encode(v)] for k, v in o.items()]}
    if isinstance(o, Path):
        return str(o)
    return o            # int / float / str / bool / None

def wire_decode(o):
    if isinstance(o, list):
        return [wire_decode(x) for x in o]
    if isinstance(o, dict):
        if set(o) == {"__b"}: return base64.b64decode(o["__b"])
        if set(o) == {"__s"}: return {wire_decode(x) for x in o["__s"]}
        if set(o) == {"__d"}:
            return {wire_decode(k): wire_decode(v) for k, v in o["__d"]}
        return {k: wire_decode(v) for k, v in o.items()}
    return o

class ChannelServer:
    """
    The UI socket (ui.sock) — the ONE socket. It serves two connection shapes:

      • CLI control ops (patch/apply/discard/select/rename/remove): a one-shot
        request/response over a short-lived connection (newline JSON, no persistent
        channel). These come from separate runner processes; the connection closes
        after the reply.

      • A BOX runner's persistent connection: the register handshake (recvmsg JSON +
        the runner's pidfd, JSON ack) followed by the MUXED box channel on the SAME
        connection — ECHO/ECHO_DONE frames UI→inner (live echo), MUTE/UNMUTE frames
        inner→UI (nested-echo mute). Connection-close is the box's teardown signal (it
        runs the consolidate the old unregister did).

    ui.sock IS bind-mounted into boxes (read-only at UI_SOCK_INBOX) so a nested runner
    can reach it. A box never registers/unregisters another session: a box is bound to
    the minted sid at register (the connection it arrived on), never trusted from inside
    the box; teardown needs no owner token (the box's own connection closing IS the
    signal).
    """
    # Verbs the remote UI may call ({"type":"ui","verb":...,"args":[...]}) —
    # plain synchronous Supervisor/ChangeReview methods, loop-confined like every
    # other control op. Anything not listed here is refused (closed surface).
    _SUP_VERBS = frozenset((
        "session_dicts", "display_path", "resolve_box", "select",
        "processes", "processes_live", "proc_prov", "proc_info", "proc_roots",
        "process_env", "writer_id", "first_writer_id", "first_writer_prov",
        "outputs", "output_detail", "open_files",
        "kill", "dissolve", "rename", "rescan", "delete"))
    _REVIEW_VERBS = frozenset((
        "session_changes", "hunks", "apply", "discard", "apply_hunk",
        "discard_hunk", "decorate", "change_mode", "patch_text",
        "invalidate_consolidation", "invalidate_struct"))

    def __init__(self, sup: Supervisor, sock_path: str):
        self.sup = sup; self.sock_path = sock_path
        self._sock = None
        self._subscribers: dict = {}      # conn -> outbox asyncio.Queue
        self._struct_jobs: dict = {}      # job_id -> (job, [spawned Popen,...])
        self._next_job = 1

    # ── engine→client event stream ───────────────────────────────────────────
    def broadcast(self, ev: dict) -> None:
        """Push one event (session lifecycle / overlay FUSE event) to every
        subscribed client as a JSON line. Loop-confined (called via sup.on_event
        and the engine's overlay drain). Each subscriber has an outbox queue and
        a pump task (partial-send-safe); a stalled one (full outbox) is dropped."""
        if not self._subscribers: return
        try: data = (json.dumps(wire_encode(ev)) + "\n").encode()
        except (TypeError, ValueError): return
        for conn, q in list(self._subscribers.items()):
            if q.qsize() > 10000:
                self._drop_subscriber(conn); continue
            q.put_nowait(data)

    def _drop_subscriber(self, conn) -> None:
        q = self._subscribers.pop(conn, None)
        if q is not None: q.put_nowait(None)        # wake the pump to exit

    async def _pump_events(self, conn) -> None:
        q = self._subscribers.get(conn)
        try:
            while True:
                data = await q.get()
                if data is None: break
                await self._loop.sock_sendall(conn, data)
        except (OSError, asyncio.CancelledError):
            pass
        finally:
            self._subscribers.pop(conn, None)
            try: conn.close()
            except OSError: pass

    async def start(self) -> None:
        try: os.unlink(self.sock_path)
        except FileNotFoundError: pass
        self._sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._sock.bind(self.sock_path)
        os.chmod(self.sock_path, 0o600)
        self._sock.listen(128)
        self._sock.setblocking(False)
        self._loop = asyncio.get_running_loop()
        self.sup.attach_loop(self._loop)
        self._task = asyncio.create_task(self._accept_loop())

    async def _accept_loop(self) -> None:
        while True:
            try:
                conn, _ = await self._loop.sock_accept(self._sock)
            except (OSError, asyncio.CancelledError):
                break
            asyncio.create_task(self._handle(conn))

    async def _handle(self, conn: socket.socket) -> None:
        # The FIRST read on each control connection uses recvmsg so we can capture
        # the optional pidfd the runner sends alongside its register JSON.  The
        # control socket accepts a single pidfd on the register connection — used
        # to derive the runner's HOST-namespace pid AND kept as the liveness handle
        # for the session's run_pidfd (dup'd into the Session by register).
        # Non-register messages (unregister/drop/select/rename/patch) carry no FD
        # and are handled normally — peer_pid stays 0 for those.
        _peer_pid = 0
        _pidfd = -1          # kept open until after _dispatch_control (register dups it)
        _adopted = False     # True once a subscribe pump owns this conn
        buf = b""
        try:
            _data, _anc, _flags, _addr = await self._loop.run_in_executor(
                None, _recvmsg_blocking, conn)
            buf = _data
            for lvl, typ, data in _anc:
                if lvl == socket.SOL_SOCKET and typ == socket.SCM_RIGHTS:
                    a = array.array("i"); a.frombytes(data)
                    fds = a.tolist()
                    if fds:
                        _pidfd = fds[0]
                    # Close any extra ancillary FDs beyond the first; we only
                    # expect one pidfd on the register connection.
                    for extra in fds[1:]:
                        try: os.close(extra)
                        except OSError: pass
            if _pidfd >= 0:
                _peer_pid = _host_pid_from_pidfd(_pidfd)
                # Do NOT close _pidfd here — pass it to _dispatch_control so
                # register can dup it into the Session as the liveness handle.
        except (OSError, asyncio.CancelledError):
            if _pidfd >= 0:
                try: os.close(_pidfd)
                except OSError: pass
            try: conn.close()
            except OSError: pass
            return
        try:
            # Control client: each line is JSON.  The first (and only, for a box) line
            # may carry the runner's pidfd (used above to derive peer_pid).  A `register`
            # produces a reply (mount path + owner token) AND then converts THIS same
            # connection into the box's muxed channel — we serve it and return.  All
            # other (CLI control) messages are one-shot request/response.
            while True:
                if b"\n" in buf:
                    line, buf = buf.split(b"\n", 1)
                    if line.strip():
                        reply = await self._dispatch_control(line, _peer_pid,
                                                             peer_pidfd=_pidfd)
                        # A subscribe converts this connection into a one-way
                        # event feed owned by its pump task (which closes it —
                        # _adopted keeps _handle's finally from closing it too).
                        # Register BEFORE the ack goes out (the ack is simply the
                        # outbox's first item) so an event broadcast right after
                        # the client sees the ack can never be lost to the race.
                        if reply is not None and reply.get("_subscribe"):
                            _adopted = True
                            q = asyncio.Queue()
                            q.put_nowait((json.dumps(reply) + "\n").encode())
                            self._subscribers[conn] = q
                            asyncio.ensure_future(self._pump_events(conn))
                            return
                        if reply is not None:
                            try:
                                await self._loop.run_in_executor(
                                    None, _send_reply, conn, reply)
                            except OSError:
                                pass
                            # A successful register converts this connection into the box
                            # channel: serve the mux, then run teardown on close.
                            if (self._is_register(line) and reply.get("ok")
                                    and reply.get("session_id")):
                                await self._serve_box_channel(
                                    conn, reply["session_id"], reply.get("owner_token", ""),
                                    buf)
                                return
                    continue
                more = await self._loop.sock_recv(conn, 4096)
                if not more:
                    if buf.strip():
                        reply = await self._dispatch_control(buf, _peer_pid,
                                                             peer_pidfd=_pidfd)
                        if reply is not None:
                            try:
                                await self._loop.run_in_executor(
                                    None, _send_reply, conn, reply)
                            except OSError:
                                pass
                    break
                if len(buf) + len(more) > 1 << 20:   # 1 MiB cap
                    break
                buf += more
        except (OSError, asyncio.CancelledError):
            pass
        finally:
            # Close _handle's own copy of the pidfd; register dup'd it if it kept it.
            if _pidfd >= 0:
                try: os.close(_pidfd)
                except OSError: pass
            if not _adopted:
                try: conn.close()
                except OSError: pass

    @staticmethod
    def _is_register(line: bytes) -> bool:
        try:
            return json.loads(line.decode()).get("type") == "register"
        except (json.JSONDecodeError, AttributeError):
            return False

    async def _recvmsg_ready(self, conn: socket.socket) -> "tuple[bytes, list]":
        """Await readability on `conn` via add_reader, then ONE non-blocking recvmsg.
        Returns (data, ancillary). (b"", []) on EOF. Raises OSError on a real error.
        Holds no thread — the read stays on the asyncio loop, so per-box channels never
        consume default-executor workers (which register/consolidate/_send_reply need)."""
        while True:
            try:
                data, anc, _flags, _addr = conn.recvmsg(
                    65536, socket.CMSG_SPACE(4 * 16))
                return data, anc
            except (BlockingIOError, InterruptedError):
                pass
            fut = self._loop.create_future()
            fd = conn.fileno()
            self._loop.add_reader(fd, lambda: fut.done() or fut.set_result(None))
            try:
                await fut
            finally:
                try: self._loop.remove_reader(fd)
                except (OSError, ValueError): pass

    async def _serve_box_channel(self, conn: socket.socket, sid: str,
                                 owner_token: str, leftover: bytes) -> None:
        """Serve the box's ONE muxed connection after a successful register. Reads
        length-prefixed typed frames (MUTE/UNMUTE inner→UI) pairing SCM_RIGHTS fds with
        the frame they rode in on, and lets the box's EchoStream write ECHO/ECHO_DONE
        frames out on the same connection. On EOF/close (box pid 1 exited) it runs the box
        teardown the old unregister used to trigger. `leftover` is any bytes already read
        past the register line (normally empty)."""
        # Wire the EchoStream's writer to this connection so captured bytes flow as ECHO
        # frames back to --inner. A write-only _ConnWriter over the SAME raw socket: we do
        # NOT layer an asyncio StreamReader here, because the inbound MUTE frames are read
        # via recvmsg below on this same fd and a transport's reader would steal them.
        conn.setblocking(False)
        writer = _ConnWriter(conn, self._loop)
        # Pids THIS connection added to the global muted set, so close removes exactly them.
        muted_here: set = set()
        try:
            self.sup.attach_box_channel(sid, writer)
            buf = leftover
            # Queue of SCM_RIGHTS fd-arrays in arrival order. --inner serializes its sends
            # and sends each FD-bearing frame (MUTE) as exactly one sendmsg, so the Nth
            # received fd-array pairs with the Nth FD-bearing frame decoded.
            pending_fds: list = []
            while True:
                # Read frames via NON-BLOCKING recvmsg gated on socket readiness
                # (add_reader). Crucially this holds NO executor thread for the box's
                # whole lifetime — a per-box blocking recvmsg in the shared default
                # executor would, with enough live boxes, starve the pool that register /
                # consolidate / _send_reply also use (a deadlock observed under load,
                # especially with nested boxes). add_reader keeps the read on the loop.
                try:
                    data, anc = await self._recvmsg_ready(conn)
                except OSError:
                    break
                if not data and not anc:
                    break                            # EOF: box pid 1 exited
                for lvl, typ, adata in anc:
                    if lvl == socket.SOL_SOCKET and typ == socket.SCM_RIGHTS:
                        a = array.array("i"); a.frombytes(adata)
                        pending_fds.append(a.tolist())
                buf += data
                frames, buf = decode_frames(buf)
                for ftype, payload in frames:
                    if ftype == FRAME_MUTE:
                        fds = pending_fds.pop(0) if pending_fds else []
                        host_pid = _host_pid_from_pidfd(fds[0]) if fds else 0
                        for fd in fds:
                            try: os.close(fd)
                            except OSError: pass
                        if host_pid > 0:
                            _MUTED_HOST_PIDS.add(host_pid)
                            muted_here.add(host_pid)
                    elif ftype == FRAME_UNMUTE:
                        for hp in list(muted_here):
                            _MUTED_HOST_PIDS.discard(hp)
                        muted_here.clear()
                if len(buf) > 1 << 20:               # 1 MiB cap on a partial frame
                    break
        except (OSError, asyncio.CancelledError):
            pass
        finally:
            for hp in muted_here:
                _MUTED_HOST_PIDS.discard(hp)
            self.sup.detach_box_channel(sid)
            if writer is not None:
                # RuntimeError too: a test loop may be closing under us; in production
                # the loop runs forever so this is only ever a clean transport close.
                try: writer.close()
                except (OSError, RuntimeError): pass
            # Teardown: the box's pid 1 closing the connection IS the unregister signal.
            try:
                await self.sup.unregister_async(
                    dict(session_id=sid, owner_token=owner_token, status="finished"),
                    self._loop)
            except Exception as e:
                print(f"slopbox(ui): box-channel teardown of {sid} failed: {e}",
                      file=sys.stderr)

    async def _dispatch_control(self, line: bytes, peer_pid: int = 0,
                               peer_pidfd: int = -1) -> "dict | None":
        try: msg = json.loads(line.decode())
        except json.JSONDecodeError: return None
        t = msg.get("type")
        if t == "register":
            try:
                # Derive the parent sid from kernel identity (pidfd → host pid → PPid
                # walk).  peer_pid was obtained from the runner's pidfd via
                # _host_pid_from_pidfd — never from SO_PEERCRED.  The box never
                # supplies this field; only kernel-trusted ancestry counts.
                msg["_derived_parent_sid"] = _derive_parent_sid(peer_pid, self.sup)
                msg["_register_host_pid"] = peer_pid   # pidfd-derived HOST pid of the
                                                       # runner — the box's true root
                msg["_register_pidfd"] = peer_pidfd    # server-injected; register dups it
                return self.sup.register(msg)          # acks with the mount path
            except Exception as e:
                return dict(ok=False, error=str(e))
        elif t == "select":
            # `slopbox NAME …`: select a box by sid/NAME (and move the UI cursor).
            return self.sup.select(msg.get("sid"))
        elif t == "rename":
            # `slopbox [NAME] rename NEW`: rename the targeted (or selected) box.
            sid = msg.get("sid") or self.sup.selected_sid
            return self.sup.rename(sid, msg.get("name") or "")
        elif t == "patch":
            # On-demand patch for a target box (explicit sid, else the UI's selection).
            # The patch needs the box consolidated; for a finished box that is the slow
            # deflate pass, so run it in the executor (awaited — the CLI is blocked on
            # this reply) instead of inline, keeping the loop free for the UI.
            try:
                sid, err = await self._resolve_and_consolidate(msg)
                if err is not None: return err
                data = self.sup.review.patch_text(sid)
                return dict(ok=True, patch=base64.b64encode(data).decode())
            except Exception as e:
                return dict(ok=False, error=str(e))
        elif t in ("apply", "discard"):
            # CLI apply/discard for a target box (explicit sid, else the UI's selection).
            # Consolidate off-loop (as `patch` does), act on the ENTIRE change set, then
            # force-delete the box — unlike the UI we delete unconditionally so the CLI op
            # is predictable (box consumed).
            try:
                sid, err = await self._resolve_and_consolidate(msg)
                if err is not None: return err
                paths = [e["path"] for e in self.sup.review.session_changes(sid)]
                if t == "apply":
                    res = self.sup.review.apply(sid, paths)
                    n = len(res.get("applied", []))
                else:
                    res = self.sup.review.discard(sid, paths)
                    n = len(res.get("discarded", []))
                self.sup.delete(sid)          # force: removes sqlar + backing
                self.sup.selected_sid = None
                return dict(ok=True, count=n, sid=sid, errors=res.get("errors", []))
            except Exception as e:
                return dict(ok=False, error=str(e))
        elif t == "subscribe":
            # Remote-UI event stream: ack, then _handle converts THIS connection
            # into a one-way event feed (see the _subscribe marker handling).
            return dict(ok=True, _subscribe=True)
        elif t == "shutdown":
            # Remote 'quit': stop the engine. SIGTERM is already wired (in
            # run_engine and Textual's App) to flip the stop event → server.stop()
            # → mount.stop(). Reply ok BEFORE signalling so the caller sees a
            # clean ack rather than a broken-pipe on the dying socket.
            try: os.kill(os.getpid(), signal.SIGTERM)
            except Exception: pass
            return dict(ok=True)
        elif t == "ui":
            return await self._dispatch_ui(msg)
        else:
            # Unrecognized message type: reply with an explicit error rather than
            # silently dropping the connection so protocol mismatches are visible.
            return dict(ok=False, error=f"unknown control type {t!r}")

    async def _dispatch_ui(self, msg: dict) -> dict:
        """Remote-UI verb call: {"type":"ui","verb":V,"args":[...],"kw":{...}}.
        Plain verbs map to whitelisted Supervisor / ChangeReview methods (sync,
        loop-confined). The slow/stateful ones (consolidate, structural diff)
        get explicit handling: executor for the slow halves, a server-side job
        registry for struct jobs (their payloads carry raw file bytes — they
        never cross the wire; the client holds an opaque job id)."""
        verb = msg.get("verb") or ""
        args = wire_decode(msg.get("args") or [])
        kw = {k: wire_decode(v) for k, v in (msg.get("kw") or {}).items()}
        try:
            if verb == "review_state":
                rv = self.sup.review
                return dict(ok=True, r=wire_encode(dict(
                    consolidating=sorted(rv._consolidating),
                    consolidated=sorted(rv._consolidated),
                    selected=self.sup.selected_sid)))
            if verb == "review_live":
                return dict(ok=True, r=self.sup.review._live(*args))
            if verb == "consolidate_start":
                # Remote counterpart of the UI's _maybe_start_consolidate: prep on
                # the loop, fold in the engine's executor, progress/done/failed as
                # broadcast events. Returns True if a fold is running (caller shows
                # a placeholder, gated by review_state), False if nothing to do.
                sid = args[0]
                rv = self.sup.review
                if sid in rv._consolidating:
                    return dict(ok=True, r=True)
                prep = rv._consolidate_prep(sid)
                if prep is None:
                    return dict(ok=True, r=False)
                rv._consolidating.add(sid)
                loop = self._loop

                def prog(done, total):
                    loop.call_soon_threadsafe(self.broadcast, dict(
                        type="consolidate_progress", sid=sid,
                        done=done, total=total))

                async def run_fold():
                    try:
                        await loop.run_in_executor(
                            None, lambda: consolidate(prep["shm_dir"], sid,
                                                      progress=prog))
                    except Exception as e:
                        rv._consolidating.discard(sid)
                        rv._consolidated.discard(sid)
                        self.broadcast(dict(type="consolidate_failed", sid=sid,
                                            error=str(e)))
                        return
                    rv._consolidate_finish(sid)
                    rv._consolidating.discard(sid)
                    self.broadcast(dict(type="consolidate_done", sid=sid))

                asyncio.ensure_future(run_fold())
                return dict(ok=True, r=True)
            if verb == "consolidate":
                # The slow deflate fold, engine-side, off the loop (same path the
                # CLI patch/apply use); the client blocks on the reply.
                await self.sup.consolidate_in_executor(self._loop, args[0])
                return dict(ok=True, r=None)
            if verb == "struct_quick":
                lines, job = self.sup.review.structural_diff_quick(*args)
                jid = None
                if job is not None:
                    jid = self._next_job; self._next_job += 1
                    self._struct_jobs[jid] = (job, [])
                return dict(ok=True, r=wire_encode(dict(lines=lines, job=jid)))
            if verb == "struct_finish":
                ent = self._struct_jobs.get(args[0])
                if ent is None:
                    return dict(ok=False, error="unknown struct job")
                job, procs = ent
                res = await self._loop.run_in_executor(
                    None, lambda: self.sup.review.structural_diff_finish(
                        job, on_spawn=procs.append))
                self._struct_jobs.pop(args[0], None)
                return dict(ok=True, r=wire_encode(res))
            if verb == "struct_cancel":
                ent = self._struct_jobs.pop(args[0], None)
                if ent is not None:
                    for p in ent[1]:
                        try: p.kill()
                        except Exception: pass
                return dict(ok=True, r=None)
            if verb.startswith("review."):
                v = verb[len("review."):]
                if v not in self._REVIEW_VERBS:
                    return dict(ok=False, error=f"unknown verb {verb!r}")
                fn = getattr(self.sup.review, v)
            elif verb in self._SUP_VERBS:
                fn = getattr(self.sup, verb)
            else:
                return dict(ok=False, error=f"unknown verb {verb!r}")
            return dict(ok=True, r=wire_encode(fn(*args, **kw)))
        except Exception as e:
            return dict(ok=False, error=f"{type(e).__name__}: {e}")

    async def _resolve_and_consolidate(self, msg: dict) -> "tuple[str | None, dict | None]":
        """Resolve the target sid from msg (explicit 'sid' field or the UI selection),
        select it, and consolidate in the executor.  Returns (sid, None) on success or
        (None, error_reply) when the sid cannot be resolved.  Raises on consolidation
        errors (the caller's try/except covers those)."""
        raw = msg.get("sid")
        if raw:
            sid = self.sup.resolve_box(raw)          # NAME / path / box_id → box_id
            if sid is None:
                return None, dict(ok=False, error=f"no slopbox '{raw}'")
            self.sup.select(sid)
        else:
            sid = self.sup.selected_sid
            if not sid:
                return None, dict(ok=False, error="no slopbox selected in the UI")
        await self.sup.consolidate_in_executor(self._loop, sid)
        return sid, None

    async def stop(self) -> None:
        if getattr(self, "_task", None):
            self._task.cancel()
        for conn in list(self._subscribers):
            self._drop_subscriber(conn)
        try:
            self.sup.stop_all_echoes()
        except Exception:
            pass
        if self._sock is not None:
            try:
                self._sock.close()
            except OSError:
                pass
        try: os.unlink(self.sock_path)
        except FileNotFoundError: pass

class RemoteError(RuntimeError):
    """A remote verb call failed: engine unreachable or the verb errored."""

class _RemoteSet:
    """Read-only set view over an engine-side set (review._consolidating /
    _consolidated), re-fetched per access. The UI only reads these remotely —
    mutations happen engine-side (consolidate_start / its completion)."""
    __slots__ = ("_fetch",)
    def __init__(self, fetch): self._fetch = fetch
    def __contains__(self, x): return x in self._fetch()
    def __iter__(self): return iter(self._fetch())
    def __bool__(self): return bool(self._fetch())
    def __len__(self): return len(self._fetch())
    def discard(self, x): pass        # engine owns membership; remote no-op

class RemoteReview:
    """Client facade for ChangeReview: whitelisted verbs become RPCs; the
    consolidation flow maps to consolidate_start + review_state + events; the
    structural diff's quick/finish split maps to the server-side job registry
    (job payloads carry raw file bytes and never cross the wire)."""
    def __init__(self, rsup: "RemoteSupervisor"):
        self._s = rsup

    def __getattr__(self, name):
        if name in ChannelServer._REVIEW_VERBS:
            return lambda *a, **k: self._s._rpc(f"review.{name}", *a, **k)
        raise AttributeError(name)

    # decorate/change_mode accept a local ChangeSource via source= in embedded
    # mode; remotely the engine resolves its own source — drop the kwarg.
    def decorate(self, sid, rel, source=None) -> dict:
        return self._s._rpc("review.decorate", sid, rel)

    def change_mode(self, sid, rel, source=None):
        return self._s._rpc("review.change_mode", sid, rel)

    def _live(self, sid) -> bool:
        return self._s._rpc("review_live", sid)

    def _state(self) -> dict:
        return self._s._rpc("review_state")

    @property
    def _consolidating(self):
        return _RemoteSet(lambda: set(self._state()["consolidating"]))

    @property
    def _consolidated(self):
        return _RemoteSet(lambda: set(self._state()["consolidated"]))

    def consolidate_start(self, sid) -> bool:
        return self._s._rpc("consolidate_start", sid)

    def structural_diff_quick(self, sid, rel) -> tuple:
        r = self._s._rpc("struct_quick", sid, rel)
        lines = [tuple(x) for x in r["lines"]]
        return (lines, r["job"])          # job: opaque id or None

    def structural_diff_finish(self, job, on_spawn=None) -> dict:
        # on_spawn is the embedded UI's cancel hook; remote cancel goes through
        # struct_cancel(job id) instead.
        return self._s._rpc("struct_finish", job)

    def struct_cancel(self, job) -> None:
        try: self._s._rpc("struct_cancel", job)
        except RemoteError: pass

class RemoteSupervisor:
    """Client-side Supervisor facade: every whitelisted verb is an RPC to the
    running engine over the control socket. Each call opens its own short-lived
    connection (sync_request), so it is thread-safe — UI worker threads can call
    it exactly like the embedded Supervisor."""
    def __init__(self, sock: "str | None" = None):
        self.sock = sock or sock_path()
        self.review = RemoteReview(self)
        self.on_event = lambda ev: None   # fed by the subscription reader
        self.mount = None                 # no local mount in remote mode
        self._selected = None

    def _rpc(self, verb, *args, **kw):
        rep = sync_request(self.sock, type="ui", verb=verb,
                           args=[wire_encode(a) for a in args],
                           kw={k: wire_encode(v) for k, v in kw.items()})
        if not rep:
            raise RemoteError(f"engine unreachable ({verb})")
        if not rep.get("ok"):
            raise RemoteError(rep.get("error") or verb)
        return wire_decode(rep.get("r"))

    def __getattr__(self, name):
        if name in ChannelServer._SUP_VERBS:
            return lambda *a, **k: self._rpc(name, *a, **k)
        raise AttributeError(name)

    @property
    def selected_sid(self):
        return self._selected

    @selected_sid.setter
    def selected_sid(self, sid):
        self._selected = sid
        if sid is not None:
            try: self._rpc("select", sid)
            except RemoteError: pass

    @property
    def sessions(self) -> dict:
        """Embedded-shape compatibility: {sid: dict} (values are plain dicts,
        which is what the UI renders from anyway)."""
        return {d["session_id"]: d for d in self._rpc("session_dicts")}

    async def sweep_orphans(self, loop=None) -> None:
        return None                       # the engine swept at ITS startup

    def request_shutdown(self) -> None:
        """Ask the engine to terminate (q's remote-quit path). Fire-and-forget:
        the engine replies ok and then SIGTERMs itself, so any error here just
        means the engine is already gone."""
        try: sync_request(self.sock, type="shutdown")
        except Exception: pass

def _capeff_has_caps(capeff_hex: str) -> bool:
    """Return True iff the CapEff hex string has CAP_SYS_ADMIN (bit 21) set.
    Pure/testable: takes the hex string."""
    try:
        bits = int(capeff_hex, 16)
    except (ValueError, TypeError):
        return False
    CAP_SYS_ADMIN = 21
    return bool(bits & (1 << CAP_SYS_ADMIN))

def _have_ambient_caps() -> bool:
    """True iff this process holds CAP_SYS_ADMIN in its effective set (parsed from
    /proc/self/status CapEff). False if unreadable."""
    try:
        with open("/proc/self/status") as f:
            for line in f:
                if line.startswith("CapEff:"):
                    return _capeff_has_caps(line.split(":", 1)[1].strip())
    except OSError:
        return False
    return False

_SANDBOX_PATH = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

def _untrusted_rlimits(timeout, out_cap):
    cpu = max(1, int(timeout) + 1)
    def _set():
        os.setpgrp()
        resource.setrlimit(resource.RLIMIT_CPU, (cpu, cpu))
        AS = 512 * 1024 * 1024
        resource.setrlimit(resource.RLIMIT_AS, (AS, AS))
        fsz = max(out_cap, 1024 * 1024)
        resource.setrlimit(resource.RLIMIT_FSIZE, (fsz, fsz))
    return _set

def run_on_untrusted(argv, files, timeout=10, out_cap=256 * 1024, on_spawn=None):
    """Run `argv` over untrusted `files` (name->bytes) inside a throwaway bwrap.
    `files` are written to a fresh temp dir, ro-bound read-only into the sandbox;
    `argv` placeholders like "{in}" resolve to the path INSIDE the sandbox.
    `on_spawn(proc)`, if given, receives the bwrap Popen right after launch so a
    caller on another thread can kill it (the box runs under --die-with-parent in its
    own pid namespace, so a SIGKILL to the group tears the whole tree down — no
    zombies). Returns (ok, output_text, err): ok False + a clean error string on
    timeout / oom / missing tool / nonzero exit / external kill. Never raises."""
    if shutil.which("bwrap") is None:
        return (False, "", "bwrap unavailable")
    try:
        with tempfile.TemporaryDirectory(prefix="ut-") as td:
            inside = "/tmp/ut"
            sub = {}
            for name, data in (files or {}).items():
                p = os.path.join(td, name)
                with open(p, "wb") as f: f.write(data)
                sub[name] = inside + "/" + name
            real = [sub.get(a.strip("{}"), a) if (a.startswith("{") and a.endswith("}"))
                    else a for a in argv]
            bw = ["bwrap", "--unshare-pid", "--unshare-ipc", "--unshare-uts",
                  "--unshare-net", "--die-with-parent", "--new-session",
                  "--cap-drop", "ALL", "--ro-bind", "/", "/",
                  "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp",
                  "--ro-bind", td, inside, "--chdir", inside,
                  "--clearenv", "--setenv", "PATH", _SANDBOX_PATH, "--"] + real
            try:
                proc = subprocess.Popen(
                    bw, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE, preexec_fn=_untrusted_rlimits(timeout, out_cap))
            except Exception as e:
                return (False, "", f"spawn failed: {e}")
            if on_spawn is not None:
                try: on_spawn(proc)
                except Exception: pass
            try:
                out, err = proc.communicate(timeout=timeout)
            except subprocess.TimeoutExpired:
                try: os.killpg(proc.pid, signal.SIGKILL)
                except Exception:
                    try: proc.kill()
                    except Exception: pass
                try: proc.communicate(timeout=2)
                except Exception: pass
                return (False, "", f"timed out after {timeout}s")
            out_t = out[:out_cap].decode("utf-8", "replace")
            if proc.returncode != 0:
                msg = (err or b"").decode("utf-8", "replace").strip()[:2000]
                if proc.returncode < 0:
                    return (False, out_t, f"killed by signal {-proc.returncode}"
                            + (f": {msg}" if msg else " (rlimit?)"))
                return (False, out_t, msg or f"exit {proc.returncode}")
            return (True, out_t, "")
    except Exception as e:
        return (False, "", f"sandbox error: {e}")
