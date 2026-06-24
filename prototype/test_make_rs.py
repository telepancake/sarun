#!/usr/bin/env python3
"""Phase 2 — embedded `make` IN-PROCESS: kati parses the Makefile → ninja graph,
embedded n2 (Phase 1) executes it, recipes run through brush — for the RUST
engine.

A -b brush box already shadows /bin/sh, ninja with the engine binary. Phase 2
extends the shadow to make/gmake (and /usr/bin/make, /bin/make): when a -b box
runs `make`, the engine drives a vendored fork of kati (github.com/google/kati
src-rs/) IN-PROCESS to PARSE the Makefile and GENERATE a ninja graph, hands that
graph PURELY IN-MEMORY (via a memfd — NO disk build.ninja temp) to the embedded
n2, which EXECUTES it routing each recipe through embedded brush in the SAME
process — NO /bin/sh fork, NO external coreutil, NO engine re-exec, NO real make.

Cases verified (all real, against the built engine binary):
  1. make builds in-process, NO /bin/sh fork: `out.txt:` / `\\techo hi > $@`
     builds out.txt → 'hi' captured, AND the process table shows NO /bin/sh and
     NO external echo for the recipe.
  2. make with a coreutil recipe (cp) runs in-process: the copy happened and
     there is NO external /usr/bin/cp.
  3. multi-target with a prerequisite (`all: a b`, a/b each a recipe) builds
     both, AND build_edges provenance lists the edges for all/a/b — INCLUDING a
     target already up to date that n2 skips (its output touched newer).
  4. memfd handoff is real: NO build.ninja (or build*.ninja) file is left on the
     box filesystem / overlay after the build (the handoff was in-memory only).
  5. an unsupported makefile construct fails VISIBLY (non-zero + stderr) — no
     silent fallback to real make.

Run:
    uv run --with "pyfuse3>=3.2" --with "trio>=0.22" --with "wcmatch>=8.4" \
      --with "python-magic>=0.4" python test_make_rs.py
Skips (passes vacuously) if cargo/the binary are unavailable.
"""
import json, os, shutil, socket, sqlite3, subprocess, sys, tempfile, time
from pathlib import Path
from importlib.machinery import SourceFileLoader

_HERE = Path(__file__).resolve().parent
SARUN = str(_HERE / "libtestsarun.py")
CRATE = _HERE.parent / "engine"
BIN = CRATE / "target/x86_64-unknown-linux-musl/release/sarun"

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


def ensure_binary() -> bool:
    if BIN.exists():
        return True
    if shutil.which("cargo") is None:
        return False
    r = subprocess.run(["make", "engine"], cwd=CRATE.parent,
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


def process_rows(sp):
    con = sqlite3.connect(f"file:{sp}?mode=ro", uri=True)
    try:
        return [(e or "", a or "") for (e, a) in con.execute(
            "SELECT exe, argv FROM process")]
    finally:
        con.close()


def count_basename(sp, util):
    """Number of DISTINCT processes (by tgid) whose exe basename == util. The
    box's top-level `make` is the shadowed engine binary (its bind-mounted
    /usr/bin/make IS the engine); a clone/exec may yield >1 row for the SAME
    tgid, so we count distinct tgids — one real make-named process."""
    con = sqlite3.connect(f"file:{sp}?mode=ro", uri=True)
    try:
        return len({tg for (exe, tg) in con.execute("SELECT exe, tgid FROM process")
                    if os.path.basename(exe or "") == util})
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


def sqlar_names(sp):
    con = sqlite3.connect(f"file:{sp}?mode=ro", uri=True)
    try:
        return [r[0] for r in con.execute("SELECT name FROM sqlar")]
    finally:
        con.close()


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


def run_make(name, work, *extra):
    return subprocess.run(
        [str(BIN), "run", "-b", name, "-C", str(work), "--", "make", *extra],
        capture_output=True, text=True, timeout=180)


def run_ninja(name, work, *extra):
    return subprocess.run(
        [str(BIN), "run", "-b", name, "-C", str(work), "--", "ninja", *extra],
        capture_output=True, text=True, timeout=180)


def edge_for(edges, base):
    """All edges whose any output's basename == base."""
    return [(o, i, c) for (o, i, c) in (edges or [])
            if any(os.path.basename(x) == base for x in o)]


def main():
    if not ensure_binary():
        raise SystemExit("test_make_rs: engine binary unavailable — run `make engine`")
    tmp = Path(tempfile.mkdtemp(prefix="makers-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "RS"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    eng = None
    # A box-absolute working dir the overlay exposes (host /tmp is tmpfs-hidden
    # box-side). Author the Makefile under /root (visible via the overlay).
    work = Path("/root/makers_work")
    shutil.rmtree(work, ignore_errors=True)
    work.mkdir(parents=True, exist_ok=True)
    try:
        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        sock = m.sock_path()
        if not wait_socket(sock):
            raise RuntimeError("rust engine socket never appeared")

        # ── CASE 1: make build in-process, NO /bin/sh fork ──────────────────
        (work / "Makefile").write_text("out.txt:\n\techo hi > $@\n")
        (work / "out.txt").unlink(missing_ok=True)
        r = run_make("MAKE1", work)
        check(r.returncode == 0,
              f"case1: make box exits 0 (got {r.returncode}: {r.stderr[-600:]})")
        sp1 = latest_sqlar(m)
        rel = str((work / "out.txt").resolve()).lstrip("/")
        check(m.sqlar_content(sp1, rel) == b"hi\n",
              f"case1: out.txt is 'hi' — kati→n2 ran the recipe through brush "
              f"(sqlar {rel!r}={m.sqlar_content(sp1, rel)!r})")
        check(not has_external(sp1, "sh"),
              "case1: NO /bin/sh process row — recipe ran in-process (no shell fork)")
        check(not has_external(sp1, "echo"),
              "case1: NO external echo process row")
        # In a -b box the top-level `make` is dispatched to the in-process make
        # BUILTIN (brush), so kati runs inside the engine process — there is NO
        # separate `make` process at all. ZERO `make` rows; any fork (a real
        # make, or a fallback) would show at least one.
        check(count_basename(sp1, "make") == 0,
              f"case1: no `make` process row — make ran fully in-process via the "
              f"builtin (count={count_basename(sp1, 'make')})")

        # ── CASE 2: coreutil recipe (cp) in-process ─────────────────────────
        (work / "src.txt").write_text("payload\n")
        (work / "copy.txt").unlink(missing_ok=True)
        (work / "Makefile").write_text("copy.txt: src.txt\n\tcp $< $@\n")
        r = run_make("MAKE2", work)
        check(r.returncode == 0,
              f"case2: cp box exits 0 (got {r.returncode}: {r.stderr[-600:]})")
        sp2 = latest_sqlar(m)
        relc = str((work / "copy.txt").resolve()).lstrip("/")
        check(m.sqlar_content(sp2, relc) == b"payload\n",
              f"case2: copy.txt is 'payload' — cp ran in-process "
              f"({m.sqlar_content(sp2, relc)!r})")
        check(not has_external(sp2, "cp"),
              "case2: NO external /usr/bin/cp process row (coreutil ran in-process)")
        check(not has_external(sp2, "sh"),
              "case2: NO /bin/sh process row for the `cp` recipe")

        # ── CASE 3: multi-target w/ prereq + build_edges incl skipped ───────
        # `all: a b` ; a and b each a recipe. Pre-build `a` newer than nothing so
        # it is up to date on the second run and n2 skips it — its edge must
        # still appear. We run make TWICE in one box (warm in-memory db is per
        # process, so the SECOND `make` in the SAME box sees `a` up to date).
        (work / "a").unlink(missing_ok=True)
        (work / "b").unlink(missing_ok=True)
        (work / "a.src").write_text("aaa\n")
        (work / "Makefile").write_text(
            "all: a b\n"
            "a: a.src\n\tcp $< $@\n"
            "b:\n\techo bbb > $@\n")
        # First make builds a and b. Then touch a's output newer than a.src and
        # run make again in the same box: a is up to date (skipped), b (no
        # prereq, always considered out of date by an empty db) rebuilds.
        r = subprocess.run(
            [str(BIN), "run", "-b", "MAKE3", "-C", str(work), "--",
             "sh", "-c", "make; echo ---; touch a; make all"],
            capture_output=True, text=True, timeout=180)
        check(r.returncode == 0,
              f"case3: multi-target box exits 0 (got {r.returncode}: {r.stderr[-600:]})")
        sp3 = latest_sqlar(m)
        rela = str((work / "a").resolve()).lstrip("/")
        relb = str((work / "b").resolve()).lstrip("/")
        check(m.sqlar_content(sp3, rela) == b"aaa\n",
              f"case3: 'a' built (cp) ({m.sqlar_content(sp3, rela)!r})")
        check(m.sqlar_content(sp3, relb) == b"bbb\n",
              f"case3: 'b' built (echo) ({m.sqlar_content(sp3, relb)!r})")
        edges = build_edges_rows(sp3)
        check(edges is not None, "case3: build_edges table exists in the sqlar")
        check(bool(edge_for(edges, "all")),
              f"case3: build_edges has the `all` (phony) edge (edges={edges!r})")
        ha = edge_for(edges, "a")
        hb = edge_for(edges, "b")
        check(bool(ha), f"case3: build_edges has the `a` edge")
        check(bool(hb), f"case3: build_edges has the `b` edge")
        if ha:
            o, i, c = ha[0]
            check(any(os.path.basename(x) == "a.src" for x in i),
                  f"case3: `a` edge records prereq a.src (ins={i!r})")
            check(c is not None and "cp" in c,
                  f"case3: `a` edge records the cp recipe (cmd={c!r})")
        # The `all` aggregate edge must list a and b as prereqs.
        if edge_for(edges, "all"):
            o, i, c = edge_for(edges, "all")[0]
            ibases = {os.path.basename(x) for x in i}
            check({"a", "b"} <= ibases,
                  f"case3: `all` edge lists a and b as prereqs (ins={i!r})")

        # ── CASE 4: memfd handoff — NO build.ninja temp left behind ─────────
        # Across ALL boxes run so far, no build.ninja / build*.ninja artifact may
        # exist on the overlay (sqlar) or the host work dir.
        for sp in (sp1, sp2, sp3):
            leaked = [n for n in sqlar_names(sp)
                      if os.path.basename(n).startswith("build")
                      and os.path.basename(n).endswith(".ninja")]
            check(not leaked,
                  f"case4: NO build*.ninja left in overlay {sp.name} (memfd "
                  f"handoff, no temp file) (leaked={leaked!r})")
        host_leak = list(work.glob("build*.ninja")) + list(work.glob("**/build*.ninja"))
        check(not host_leak,
              f"case4: NO build*.ninja on the host work dir (leaked={host_leak!r})")
        # Also no kati companion artifacts (ninja.sh / .kati_stamp) leaked.
        for sp in (sp1, sp2, sp3):
            kati_leak = [n for n in sqlar_names(sp)
                         if os.path.basename(n) in ("ninja.sh", ".kati_stamp")
                         or os.path.basename(n).startswith(".kati_stamp")]
            check(not kati_leak,
                  f"case4: NO kati ninja.sh/.kati_stamp in overlay {sp.name} "
                  f"(leaked={kati_leak!r})")

        # ── CASE 5: unsupported makefile construct fails VISIBLY ────────────
        # A makefile that references an undefined user function via $(call ...)
        # to a missing macro is tolerated by make; instead use a construct kati
        # genuinely rejects: a recipe line before any target (commands commence
        # before first target rule), which GNU make and kati both error on.
        (work / "Bad.mk").write_text("\techo orphaned recipe\n")
        r = run_make("MAKE5", work, "-f", "Bad.mk")
        check(r.returncode != 0,
              f"case5: bad makefile → non-zero exit (got {r.returncode})")
        check(bool(r.stderr.strip()),
              f"case5: bad makefile → visible stderr (stderr={r.stderr[-400:]!r})")
        # The top-level `make` shadow ran (and failed visibly); a fallback would
        # have forked a SECOND real make. Exactly one make row ⇒ no fallback.
        sp5 = latest_sqlar(m)
        check(count_basename(sp5, "make") <= 1,
              f"case5: at most one `make` row (the top-level shadow) — NO real "
              f"`make` forked as fallback (count={count_basename(sp5, 'make')})")

        # ── CASE 6: recursive $(MAKE) stays IN-PROCESS via the make builtin ──
        # The top-level make (the box's shadowed engine) runs a recipe that
        # invokes a sub-make. brush dispatches that `make` to the in-process
        # MakeBuiltin instead of exec'ing the shadowed /usr/bin/make — so the
        # sub-make runs in THIS process: exactly ONE `make` row total (not two),
        # and the sub-make's recipe runs correctly at the right directory.
        (work / "subout.txt").unlink(missing_ok=True)
        (work / "sub.mk").write_text("sub:\n\techo subok > subout.txt\n")
        (work / "Makefile").write_text("all:\n\t$(MAKE) -f sub.mk\n")
        r = run_make("MAKE6", work)
        check(r.returncode == 0,
              f"case6: recursive make box exits 0 (got {r.returncode}: {r.stderr[-600:]})")
        sp6 = latest_sqlar(m)
        rels = str((work / "subout.txt").resolve()).lstrip("/")
        check(m.sqlar_content(sp6, rels) == b"subok\n",
              f"case6: sub-make ran in-process (subout.txt='subok') "
              f"({m.sqlar_content(sp6, rels)!r})")
        # ZERO make rows: BOTH the top-level make and the recursive $(MAKE)
        # dispatched to the in-process builtin. Without the builtin the recursive
        # $(MAKE) would fork a second engine-as-make (a `make` row); with it the
        # whole recursive build stays in one process.
        check(count_basename(sp6, "make") == 0,
              f"case6: recursive $(MAKE) stayed in-process — no `make` process "
              f"row (count={count_basename(sp6, 'make')}); a fork would show one")

        # ── CASE 7: self-generating include (remake) handled IN-PROCESS ─────
        # `include gen.mk` where gen.mk doesn't exist but has a rule: GNU make
        # builds gen.mk, then re-parses the makefile with it visible. The shadow
        # path re-execs the engine; the builtin does it in-process (build the
        # include, drop the makefile cache, re-run kati). Proof: GENVAR defined
        # ONLY in the generated gen.mk reaches the `all` recipe on the reparse.
        (work / "out7.txt").unlink(missing_ok=True)
        (work / "gen.mk").unlink(missing_ok=True)
        (work / "Makefile").write_text(
            "include gen.mk\n"
            "all:\n\techo $(GENVAR) > out7.txt\n"
            "gen.mk:\n\techo GENVAR := remade > gen.mk\n")
        r = run_make("MAKE7", work)
        check(r.returncode == 0,
              f"case7: remake box exits 0 (got {r.returncode}: {r.stderr[-600:]})")
        sp7 = latest_sqlar(m)
        rel7 = str((work / "out7.txt").resolve()).lstrip("/")
        check(m.sqlar_content(sp7, rel7) == b"remade\n",
              f"case7: reparse saw the generated include (GENVAR='remade') "
              f"({m.sqlar_content(sp7, rel7)!r})")
        check(count_basename(sp7, "make") == 0,
              f"case7: remake stayed in-process — no `make` process row "
              f"(count={count_basename(sp7, 'make')})")

        # ── CASE 8: top-level `ninja` runs IN-PROCESS via the ninja builtin ──
        # brush dispatches `ninja` to the in-process n2 builtin: the build runs
        # in the engine process (zero `ninja` rows), the recipe goes through
        # brush (no /bin/sh), and the output file is produced.
        (work / "nout.txt").unlink(missing_ok=True)
        (work / "build.ninja").write_text(
            "rule e\n  command = echo ninjaok > $out\n"
            "build nout.txt: e\n")
        r = run_ninja("NINJA8", work)
        check(r.returncode == 0,
              f"case8: ninja box exits 0 (got {r.returncode}: {r.stderr[-600:]})")
        sp8 = latest_sqlar(m)
        rel8 = str((work / "nout.txt").resolve()).lstrip("/")
        check(m.sqlar_content(sp8, rel8) == b"ninjaok\n",
              f"case8: ninja built nout.txt in-process ({m.sqlar_content(sp8, rel8)!r})")
        check(not has_external(sp8, "sh"),
              "case8: NO /bin/sh process row — ninja recipe ran in-process")
        check(count_basename(sp8, "ninja") == 0,
              f"case8: no `ninja` process row — ran via the in-process builtin "
              f"(count={count_basename(sp8, 'ninja')})")
    finally:
        if eng is not None and eng.poll() is None:
            eng.terminate()
            try: eng.wait(timeout=10)
            except Exception: eng.kill()
        os.environ.pop("SLOPBOX_NS", None)
        shutil.rmtree(tmp, ignore_errors=True)
        shutil.rmtree("/root/makers_work", ignore_errors=True)
    print("\n" + ("MAKE-RS PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_make_rs():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
