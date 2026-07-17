#!/usr/bin/env python3
"""Editor-pane save path through the REAL engine socket: the
`review.write_file` UI verb — what Ctrl-S on an 'E'-opened Changes file
sends — overwrites a box path's CURRENT bytes as a CAPTURED row (the
same copy_up → pool blob → finalize path the box's own writes take),
and the box's overlay serves the new bytes back: `review.file_bytes`
round-trips them byte-exact, and a CHILD box parented on the session
reads them through a real FUSE mount (`cat` sees the edit). The host
file underneath is never touched. Refusals are loud: symlinks, binary
payloads, and paths that exist nowhere all return {ok:false, error}.

Needs FUSE + bwrap (spawns real boxes). Run:
    uv run --with "wcmatch>=8.4" --with "python-magic>=0.4" \\
      python test_editor_rs.py
"""
import base64, os, shutil, socket, subprocess, tempfile, time
from pathlib import Path
from sarun_test_paths import ENGINE_BIN
from importlib.machinery import SourceFileLoader

_HERE = Path(__file__).resolve().parent
SARUN = str(_HERE / "libtestsarun.py")
CRATE = _HERE.parent / "engine"
BIN = ENGINE_BIN

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


def ensure_binary() -> bool:
    if not BIN.exists():
        r = subprocess.run(["make", "engine"], cwd=CRATE.parent,
                           capture_output=True, text=True)
        if r.returncode != 0 or not BIN.exists():
            return False
    return True


def wait_socket(sock, timeout=30):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.settimeout(1.0); s.connect(sock); return True
        except OSError:
            time.sleep(0.1)
    return False


def newest_sqlar():
    return max(Path(os.environ["XDG_STATE_HOME"]).joinpath("slopbox.EDT")
               .glob("*.sqlar"), key=lambda p: int(p.stem))


def main():
    if not ensure_binary():
        raise SystemExit("test_editor_rs: engine binary unavailable")
    tmp = Path(tempfile.mkdtemp(prefix="edt-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "EDT"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    eng = None
    try:
        # A host-side file the box never touches (the shadow-a-host-file
        # branch: the edit must land as a captured row, host unchanged).
        host_doc = tmp / "host-conf.yaml"
        host_doc.write_text("key: original\n")

        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if not wait_socket(m.sock_path()):
            out = eng.stdout.read(2000) if eng.stdout else b""
            raise RuntimeError("rust engine socket never appeared:\n"
                               + out.decode(errors="replace"))
        sock = m.sock_path()

        # The box writes a python file (the captured-write branch) and
        # plants a symlink (the refusal branch). At / — not under the test
        # tmp dir: the box's /tmp is its own tmpfs, while root writes are
        # plain captured overlay rows.
        original = "def answer():\n    return 41\n"
        r = subprocess.run(
            [str(BIN), "run", "EDIT", "--", "sh", "-c",
             r"printf 'def answer():\n    return 41\n' > /ed-code.py"
             " && ln -s /etc/hosts /ed-link"],
            capture_output=True, text=True, timeout=60)
        check(r.returncode == 0,
              f"box run exits 0 (rc={r.returncode}: {r.stderr[:200]})")
        sid = int(newest_sqlar().stem)

        def verb(name, *args):
            rep = m.sync_request(sock, type="ui", verb=name,
                                 args=[str(a) for a in args]) or {}
            return rep.get("r") or {}

        def b64(data: bytes) -> str:
            return base64.b64encode(data).decode()

        # 0. sanity: the captured row serves the box's own bytes.
        v = verb("review.file_bytes", sid, "ed-code.py")
        check(v.get("ok") is True
              and base64.b64decode(v.get("b64", "")) == original.encode(),
              f"pre-edit bytes are the box's write (got {v!r})")

        # 1. the editor save: overwrite the captured row's bytes.
        edited = "def answer():\n    return 42  # fixed in the editor\n"
        v = verb("review.write_file", sid, "ed-code.py", b64(edited.encode()))
        check(v.get("ok") is True, f"write_file succeeds (got {v!r})")

        # 2. the captured row's bytes CHANGED — file_bytes round-trips the
        #    edit byte-exact (the row, not some side copy).
        v = verb("review.file_bytes", sid, "ed-code.py")
        got = base64.b64decode(v.get("b64", "")) if v.get("ok") else b""
        check(got == edited.encode(),
              f"captured row serves the edited bytes (got {got[:60]!r})")

        # 3. the box serves the edit back THROUGH THE MOUNT: a child box
        #    parented on the session (the numeric-id-prefix stacking) reads
        #    the file via its real FUSE-mounted merged view.
        r = subprocess.run(
            [str(BIN), "run", f"{sid}.CHK", "--", "cat", "/ed-code.py"],
            capture_output=True, text=True, timeout=60)
        check(r.returncode == 0,
              f"child box run exits 0 (rc={r.returncode}: {r.stderr[:200]})")
        check(edited in r.stdout,
              f"child box cat sees the edit through the mount "
              f"(got {r.stdout[:80]!r})")

        # 4. editing a HOST file outside the change set: the edit lands as
        #    a NEW captured row; the host file is untouched.
        host_rel = str(host_doc).lstrip("/")
        v = verb("review.write_file", sid, host_rel, b64(b"key: edited\n"))
        check(v.get("ok") is True, f"host-shadow write succeeds (got {v!r})")
        v = verb("review.file_bytes", sid, host_rel)
        got = base64.b64decode(v.get("b64", "")) if v.get("ok") else b""
        check(got == b"key: edited\n",
              f"box view serves the shadowed edit (got {got!r})")
        check(host_doc.read_bytes() == b"key: original\n",
              "the HOST file is untouched by the box-layer save")

        # 5. loud refusals: symlink, binary payload, nowhere-path.
        v = verb("review.write_file", sid, "ed-link", b64(b"x"))
        check(v.get("ok") is False and "symlink" in str(v.get("error", "")),
              f"symlink refused loudly (got {v!r})")
        v = verb("review.write_file", sid, "ed-code.py", b64(b"a\0b"))
        check(v.get("ok") is False and "binary" in str(v.get("error", "")),
              f"NUL payload refused loudly (got {v!r})")
        nowhere = str(tmp / "no-such.py").lstrip("/")
        v = verb("review.write_file", sid, nowhere, b64(b"x"))
        check(v.get("ok") is False and v.get("error"),
              f"nowhere-path refused loudly (got {v!r})")
        # ...and none of the refusals corrupted the good row.
        v = verb("review.file_bytes", sid, "ed-code.py")
        got = base64.b64decode(v.get("b64", "")) if v.get("ok") else b""
        check(got == edited.encode(), "refusals left the row intact")

        # 6. BUG 2: the oaita agent's box_file_write verb now shares the
        #    editor-save refusal gate (it had NONE before), but — unlike the
        #    editor — MAY create new files. Its reply is RAW (the verb
        #    `return`s the result, so a refusal surfaces as an envelope error
        #    to the executor, not a silently-wrapped ok), so read it directly.
        def bfw(path, data):
            return m.sync_request(sock, type="ui", verb="box_file_write",
                                  args=[str(sid), path, b64(data)]) or {}
        rep = bfw("ed-link", b"x")
        check(rep.get("ok") is False and "symlink" in str(rep.get("error", "")),
              f"box_file_write refuses a symlink (got {rep!r})")
        rep = bfw("ed-code.py", b"a\0b")
        check(rep.get("ok") is False and "binary" in str(rep.get("error", "")),
              f"box_file_write refuses a NUL payload (got {rep!r})")
        # …but CREATES a brand-new file (the one documented difference: the
        #    editor refuses a nowhere-path, the agent tool authors it).
        rep = bfw("bfw-new.txt", b"created by the agent tool\n")
        check(rep.get("ok") is True,
              f"box_file_write creates a new file (got {rep!r})")
        v = verb("review.file_bytes", sid, "bfw-new.txt")
        got = base64.b64decode(v.get("b64", "")) if v.get("ok") else b""
        check(got == b"created by the agent tool\n",
              f"the agent-created file reads back (got {got!r})")
    finally:
        if eng:
            eng.terminate()
            try: eng.wait(timeout=5)
            except subprocess.TimeoutExpired: eng.kill()
        shutil.rmtree(tmp, ignore_errors=True)

    if _fails:
        raise AssertionError(_fails)
    print("test_editor_rs: all checks passed")


def main_rerun_after_discard():
    """BUG 1 (data loss): a RE-RUN box writing a file whose hunks were
    discarded must SUCCEED. `review.discard_hunk` reverts the row INLINE
    (bytes in the sqlar `data` column, the pool blob dropped); the box's
    own copy_up sourced writes from the blob, so a re-run's write to that
    path failed ENOENT against its own history — the capture path broke.
    The root fix re-materializes an inline row in copy_up for EVERY writer
    (a live FUSE write and the editor-save verb alike), so the re-run
    writes fine and a child box reads the new bytes through the mount."""
    _fails.clear()  # isolate from the sibling test (shared module global)
    if not ensure_binary():
        raise SystemExit("test_editor_rs: engine binary unavailable")
    tmp = Path(tempfile.mkdtemp(prefix="edt-rr-"))
    # The host file the box MODIFIES must live OUTSIDE /tmp — the box's /tmp
    # is its own tmpfs, so a lower file there is invisible to it. /home is a
    # real, non-shadowed lower.
    host_root = Path(tempfile.mkdtemp(prefix="rr-e2e-", dir="/home/user"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "EDT"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    eng = None
    try:
        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        if not wait_socket(m.sock_path()):
            out = eng.stdout.read(2000) if eng.stdout else b""
            raise RuntimeError("rust engine socket never appeared:\n"
                               + out.decode(errors="replace"))
        sock = m.sock_path()

        def verb(name, *args):
            rep = m.sync_request(sock, type="ui", verb=name,
                                 args=[str(a) for a in args]) or {}
            return rep.get("r") or {}

        # A HOST file under the change set's reach: the box MODIFIES it (not
        # creates), so a partial revert leaves the row PRESENT (not consumed
        # by `settle`, which only drops a row that reverts fully to its
        # lower). Two far-apart line changes → two hunks; discarding ONE
        # leaves the other, so the reverted row survives — INLINE, blob
        # dropped: the exact state a re-run's copy_up used to choke on.
        base_lines = [f"L{i:02d}" for i in range(1, 21)]          # L01..L20
        host_src = host_root / "rr-src.txt"
        host_src.write_text("\n".join(base_lines) + "\n")
        rel = str(host_src).lstrip("/")
        host_before = host_src.read_bytes()

        # box overwrites it changing L02 and L18 (16 lines apart → 2 hunks).
        mod1 = list(base_lines); mod1[1] = "X02"; mod1[17] = "X18"
        r = subprocess.run(
            [str(BIN), "run", "RRUN", "--", "sh", "-c",
             "printf '%s\\n' " + " ".join(mod1) + " > " + str(host_src)],
            capture_output=True, text=True, timeout=60)
        check(r.returncode == 0,
              f"first box run exits 0 (rc={r.returncode}: {r.stderr[:200]})")
        sid = int(newest_sqlar().stem)

        h = verb("review.hunks", sid, rel)
        check(h.get("is_text") is True and len(h.get("hunks") or []) == 2,
              f"the modification has two discardable hunks (got {h!r})")

        # discard ONLY hunk 0 (the L02 change) → L18 change remains, so the
        # row stays present and is reverted INLINE with its pool blob dropped.
        rep = m.sync_request(sock, type="ui", verb="review.discard_hunk",
                             args=[str(sid), rel, 0]) or {}
        d = rep.get("r") or {}
        check(d.get("ok") is True, f"discard_hunk succeeds (got {d!r})")
        reverted = list(base_lines); reverted[17] = "X18"        # L02 back, L18 kept
        v = verb("review.file_bytes", sid, rel)
        got = base64.b64decode(v.get("b64", "")).decode() if v.get("ok") else None
        check(got == "\n".join(reverted) + "\n",
              f"reverted row is present and serves inline bytes (got {got!r})")

        # RE-RUN the SAME named box, writing the SAME path again. Before the
        # fix this FAILED: copy_up could not source the inline row's missing
        # blob, so the box's own write errored (rc != 0). Change L05.
        mod2 = list(base_lines); mod2[4] = "Y05"; mod2[17] = "X18"
        r = subprocess.run(
            [str(BIN), "run", "RRUN", "--", "sh", "-c",
             "printf '%s\\n' " + " ".join(mod2) + " > " + str(host_src)],
            capture_output=True, text=True, timeout=60)
        check(r.returncode == 0,
              f"RE-RUN box write succeeds (rc={r.returncode}: {r.stderr[:200]})")
        sid2 = int(newest_sqlar().stem)
        check(sid2 == sid, f"re-run reuses the box id (sid={sid} sid2={sid2})")

        # the captured row now serves the re-run bytes byte-exact.
        v = verb("review.file_bytes", sid, rel)
        got = base64.b64decode(v.get("b64", "")).decode() if v.get("ok") else ""
        check(got == "\n".join(mod2) + "\n",
              f"captured row serves the re-run bytes (got {got[:60]!r})")

        # …and a CHILD box parented on the session reads them through its
        # real FUSE-mounted merged view.
        r = subprocess.run(
            [str(BIN), "run", f"{sid}.RCHK", "--", "cat", str(host_src)],
            capture_output=True, text=True, timeout=60)
        check(r.returncode == 0,
              f"child box cat exits 0 (rc={r.returncode}: {r.stderr[:200]})")
        check("Y05" in r.stdout and "X18" in r.stdout,
              f"child box sees the re-run bytes through the mount "
              f"(got {r.stdout[:120]!r})")
        # the HOST file is never touched by any box-layer write.
        check(host_src.read_bytes() == host_before,
              "the HOST file is untouched by the box writes")
    finally:
        if eng:
            eng.terminate()
            try: eng.wait(timeout=5)
            except subprocess.TimeoutExpired: eng.kill()
        shutil.rmtree(tmp, ignore_errors=True)
        shutil.rmtree(host_root, ignore_errors=True)

    if _fails:
        raise AssertionError(_fails)
    print("test_editor_rs (rerun-after-discard): all checks passed")


def test_editor_rs():
    main()


def test_editor_rs_rerun_after_discard():
    main_rerun_after_discard()


if __name__ == "__main__":
    main()
    main_rerun_after_discard()
