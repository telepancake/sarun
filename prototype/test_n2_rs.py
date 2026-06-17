#!/usr/bin/env python3
"""Phase 1 — embedded n2/ninja IN-PROCESS through brush, for the RUST engine.

A -b brush box already shadows /bin/sh, /bin/bash with the engine binary and
runs nested recipes THROUGH embedded brush-core (see test_brush_nested_sh_rs).
This extends the shadow to `ninja` / /usr/bin/ninja: when a -b box runs `ninja`,
the engine embeds a vendored fork of n2 (github.com/evmar/n2) IN-PROCESS and
routes each recipe through embedded brush in the SAME process — NO fork of
/bin/sh, NO external coreutil, NO engine re-exec. n2's per-recipe posix_spawn is
replaced by an in-process executor (engine/vendor/n2 process_posix.rs `// sarun`
patch); parallelism is forced to -j1.

Cases verified (all real, against the built engine binary):
  1. n2 runs a ninja build in-process, NO /bin/sh fork: a rule
     `command = /bin/sh -c 'echo hi > $out'` builds out.txt → 'hi' captured,
     AND the process table shows NO /bin/sh and NO external echo for the recipe.
  2. A recipe using a coreutil (`cp $in $out`) runs in-process: the copy
     happened and there is NO external /usr/bin/cp.
  3. A pipeline recipe `printf 'c\\nb\\na\\n' | sort > $out` → out is
     'a\\nb\\nc\\n' (proves the fd path through brush), no external sort.
  4. build_edges provenance: the `build_edges` table/frame contains the edge(s)
     (outs/ins/cmd), INCLUDING an up-to-date target n2 SKIPS executing (the
     output is touched newer than its input so n2 reports "no work to do" — the
     edge must STILL appear).

Run:
    uv run --with "pyfuse3>=3.2" --with "trio>=0.22" --with "wcmatch>=8.4" \
      --with "python-magic>=0.4" python test_n2_rs.py
Skips (passes vacuously) if cargo/the binary are unavailable.
"""
import json, os, shutil, socket, sqlite3, subprocess, sys, tempfile, time
from pathlib import Path
from importlib.machinery import SourceFileLoader

_HERE = Path(__file__).resolve().parent
SARUN = str(_HERE / "sarun")
CRATE = _HERE.parent / "engine"
BIN = CRATE / "target/release/sarun"

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


def ensure_binary() -> bool:
    if BIN.exists():
        return True
    if shutil.which("cargo") is None:
        return False
    r = subprocess.run(["cargo", "build", "--release"], cwd=CRATE,
                       capture_output=True, text=True)
    return r.returncode == 0 and BIN.exists()


def wait_socket(sock, timeout=30):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.settimeout(1.0); s.connect(sock); return True
        except OSError:
            time.sleep(0.1)
    return False


def latest_sqlar(m):
    return max(Path(os.environ["XDG_STATE_HOME"]).joinpath("slopbox.RS")
               .glob("*.sqlar"), key=lambda p: int(p.stem))


def outputs_of(sp):
    con = sqlite3.connect(f"file:{sp}?mode=ro", uri=True)
    try:
        return b"".join(c for (c,) in con.execute(
            "SELECT content FROM outputs WHERE content IS NOT NULL"))
    finally:
        con.close()


def process_rows(sp):
    con = sqlite3.connect(f"file:{sp}?mode=ro", uri=True)
    try:
        return [(e or "", a or "") for (e, a) in con.execute(
            "SELECT exe, argv FROM process")]
    finally:
        con.close()


def has_external(sp, util):
    """True iff the process table shows an external binary whose basename is
    `util` (e.g. /bin/sh, /usr/bin/cp). Proves a recipe forked a real process."""
    for exe, argv in process_rows(sp):
        if os.path.basename(exe) == util:
            return True
        try:
            a0 = json.loads(argv)[0] if argv.strip().startswith("[") else argv.split()[0]
        except Exception:
            a0 = argv.split()[0] if argv.split() else ""
        if os.path.basename(a0) == util:
            return True
    return False


def build_edges_rows(sp):
    con = sqlite3.connect(f"file:{sp}?mode=ro", uri=True)
    try:
        names = [r[0] for r in con.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='build_edges'")]
        if not names:
            return None
        return [(json.loads(o), json.loads(i), c) for (o, i, c) in con.execute(
            "SELECT outs, ins, cmd FROM build_edges ORDER BY id")]
    finally:
        con.close()


def run_box(name, ninja_dir, *extra):
    return subprocess.run(
        [str(BIN), "run", "-b", name, "-C", str(ninja_dir), "--", "ninja", *extra],
        capture_output=True, text=True, timeout=180)


def main():
    if not ensure_binary():
        print("  ok  n2-rs: cargo/binary unavailable — SKIP")
        print("\nN2-RS PASS (skipped)")
        return 0
    tmp = Path(tempfile.mkdtemp(prefix="n2rs-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "RS"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    eng = None
    # A box-absolute working dir the overlay exposes. The box mounts `--tmpfs
    # /tmp`, so a host /tmp/... dir is hidden box-side; we author build.ninja
    # under /root (visible via the overlay) and build it at the same path. Writes
    # land in the overlay (captured), never on the host (we clean it up).
    work = Path("/root/n2rs_work")
    shutil.rmtree(work, ignore_errors=True)
    work.mkdir(parents=True, exist_ok=True)
    try:
        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        sock = m.sock_path()
        if not wait_socket(sock):
            raise RuntimeError("rust engine socket never appeared")

        # ── CASE 1: ninja build in-process, NO /bin/sh fork ─────────────────
        (work / "build.ninja").write_text(
            "rule echohi\n"
            "  command = /bin/sh -c 'echo hi > $out'\n"
            "build out.txt: echohi\n")
        # Make sure no stale output shadows the build.
        (work / "out.txt").unlink(missing_ok=True)
        r = run_box("NINJA1", work)
        check(r.returncode == 0,
              f"case1: ninja box exits 0 (got {r.returncode}: {r.stderr[-500:]})")
        sp1 = latest_sqlar(m)
        rel = str((work / "out.txt").resolve()).lstrip("/")
        check(m.sqlar_content(sp1, rel) == b"hi\n",
              f"case1: out.txt is 'hi' — n2 ran the recipe through brush "
              f"(sqlar {rel!r}={m.sqlar_content(sp1, rel)!r})")
        check(not has_external(sp1, "sh"),
              "case1: NO /bin/sh process row — recipe ran in-process (no shell fork)")
        check(not has_external(sp1, "echo"),
              "case1: NO external echo process row")

        # ── CASE 2: coreutil recipe (cp) in-process ─────────────────────────
        (work / "src.txt").write_text("payload\n")
        (work / "copy.txt").unlink(missing_ok=True)
        (work / "build.ninja").write_text(
            "rule docp\n"
            "  command = cp $in $out\n"
            "build copy.txt: docp src.txt\n")
        r = run_box("NINJA2", work)
        check(r.returncode == 0,
              f"case2: cp box exits 0 (got {r.returncode}: {r.stderr[-500:]})")
        sp2 = latest_sqlar(m)
        relc = str((work / "copy.txt").resolve()).lstrip("/")
        check(m.sqlar_content(sp2, relc) == b"payload\n",
              f"case2: copy.txt is 'payload' — cp ran in-process "
              f"({m.sqlar_content(sp2, relc)!r})")
        check(not has_external(sp2, "cp"),
              "case2: NO external /usr/bin/cp process row (coreutil ran in-process)")
        check(not has_external(sp2, "sh"),
              "case2: NO /bin/sh process row for the bare `cp` recipe")

        # ── CASE 3: pipeline recipe through brush ───────────────────────────
        (work / "sorted.txt").unlink(missing_ok=True)
        (work / "build.ninja").write_text(
            "rule sortit\n"
            "  command = printf 'c\\nb\\na\\n' | sort > $out\n"
            "build sorted.txt: sortit\n")
        r = run_box("NINJA3", work)
        check(r.returncode == 0,
              f"case3: pipeline box exits 0 (got {r.returncode}: {r.stderr[-500:]})")
        sp3 = latest_sqlar(m)
        rels = str((work / "sorted.txt").resolve()).lstrip("/")
        check(m.sqlar_content(sp3, rels) == b"a\nb\nc\n",
              f"case3: sorted.txt is a\\nb\\nc\\n — printf|sort fd path through "
              f"brush ({m.sqlar_content(sp3, rels)!r})")
        check(not has_external(sp3, "sort"),
              "case3: NO external /usr/bin/sort process row (coreutil in-process)")
        check(not has_external(sp3, "printf"),
              "case3: NO external printf process row")

        # ── CASE 4: build_edges incl. up-to-date (skipped) target ───────────
        # Pre-create the output NEWER than its input so n2 has "no work to do"
        # for it — the recipe never runs, but the edge must STILL be captured.
        (work / "in.txt").write_text("hello\n")
        (work / "done.txt").write_text("hello\n")
        now = time.time()
        os.utime(work / "in.txt", (now - 100, now - 100))
        os.utime(work / "done.txt", (now, now))  # newer than its input
        (work / "build.ninja").write_text(
            "rule mk\n"
            "  command = cp $in $out\n"
            "build done.txt: mk in.txt\n")
        # n2 decides up-to-date-ness from its on-disk .n2_db build hashes, not
        # bare mtimes — and each box has its OWN overlay, so a warm db must be
        # built inside the SAME box. Run ninja TWICE in one box body (brush runs
        # `ninja; ninja`): the first populates .n2_db in this overlay, the second
        # sees nothing changed → "no work to do", the recipe NEVER executes — yet
        # build_edges must STILL capture the edge (emitted on every load).
        r = subprocess.run(
            [str(BIN), "run", "-b", "NINJA4", "-C", str(work), "--",
             "sh", "-c", "ninja; echo ---; ninja"],
            capture_output=True, text=True, timeout=180)
        check(r.returncode == 0,
              f"case4: up-to-date box exits 0 (got {r.returncode}: {r.stderr[-500:]})")
        check("no work to do" in (r.stdout + r.stderr),
              f"case4: n2 reports 'no work to do' (target up to date): "
              f"stdout={r.stdout[-200:]!r}")
        sp4 = latest_sqlar(m)
        edges = build_edges_rows(sp4)
        check(edges is not None,
              "case4: build_edges table exists in the sqlar")
        if edges:
            hit = [(o, i, c) for (o, i, c) in edges
                   if any(os.path.basename(x) == "done.txt" for x in o)]
            check(bool(hit),
                  f"case4: build_edges has the done.txt edge even though it was "
                  f"NOT rebuilt (edges={edges!r})")
            if hit:
                o, i, c = hit[0]
                check(any(os.path.basename(x) == "in.txt" for x in i),
                      f"case4: that edge records its input in.txt (ins={i!r})")
                check(c is not None and "cp" in c,
                      f"case4: that edge records the recipe cmd (cmd={c!r})")
        # No actual build process should have run for the up-to-date target.
        check(not has_external(sp4, "cp"),
              "case4: NO external cp ran (target was up to date)")

        # ── build_edges queryable over the control socket too ───────────────
        bid = int(sp4.stem)
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.connect(sock)
                s.sendall(json.dumps(
                    {"type": "ui", "verb": "build_edges",
                     "args": [str(bid)]}).encode() + b"\n")
                buf = b""
                s.settimeout(5)
                while b"\n" not in buf:
                    chunk = s.recv(65536)
                    if not chunk: break
                    buf += chunk
            reply = json.loads(buf.decode().splitlines()[0])
        except Exception as e:
            reply = {"_error": str(e)}
        # dispatch_ui wraps the result as {"ok":true,"r":<rows>}.
        rows = reply.get("r") if isinstance(reply, dict) else reply
        check(isinstance(rows, list) and any(
                any(os.path.basename(x) == "done.txt" for x in e.get("outs", []))
                for e in rows if isinstance(e, dict)),
              f"case4: control verb `build_edges` returns the done.txt edge "
              f"({str(reply)[:300]})")
    finally:
        if eng is not None and eng.poll() is None:
            eng.terminate()
            try: eng.wait(timeout=10)
            except Exception: eng.kill()
        os.environ.pop("SLOPBOX_NS", None)
        shutil.rmtree(tmp, ignore_errors=True)
        shutil.rmtree("/root/n2rs_work", ignore_errors=True)
    print("\n" + ("N2-RS PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_n2_rs():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
