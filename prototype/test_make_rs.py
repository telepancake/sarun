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
    # Pin the engine-global slip pool to 1 so the parallelism/reaping cases are
    # deterministic: n2's implicit token + this 1 pool slip → peak 2 under -j2
    # (case 12), and a single leaked slip exhausts the pool (case 15).
    os.environ["SARUN_JOBS"] = "1"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    eng = None
    # A box-absolute working dir the overlay exposes (host /tmp is tmpfs-hidden
    # box-side). Keep it beneath the invoking user's home; hard-coding /root
    # made the native aarch64 fixture fail before it ever launched Sarun.
    work = Path.home() / "makers_work"
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

        # ── CASE 9: failing recipe in a NESTED make — engine survives ───────
        # A recursive sub-make whose recipe fails. kati used std::process::exit
        # on recipe failure, which (as an in-process builtin) would kill the
        # whole engine; it now propagates BuildFailed instead. So: the box exits
        # non-zero CLEANLY, the `*** [target] Error N` line is routed up (fd 2),
        # and the sub-make stayed in-process (no `make` row, and no engine death).
        (work / "fail.mk").write_text("boom:\n\tfalse\n")
        (work / "Makefile").write_text("all:\n\t$(MAKE) -f fail.mk\n")
        r = run_make("MAKE9", work)
        check(r.returncode != 0,
              f"case9: failing nested make → non-zero exit (got {r.returncode})")
        out_err = r.stdout + r.stderr
        check("Error 1" in out_err or "Error 2" in out_err,
              f"case9: recipe-failure line routed up (out+err tail={out_err[-300:]!r})")
        sp9 = latest_sqlar(m)
        check(count_basename(sp9, "make") == 0,
              f"case9: nested make stayed in-process; engine survived the failure "
              f"(make rows={count_basename(sp9, 'make')})")

        # ── CASE 10: `ninja -C subdir` — logical cwd, no engine chdir ────────
        # The build dir differs from the box command's cwd. The in-process n2
        # builtin must stat inputs, read build.ninja, and run the recipe against
        # `subdir` WITHOUT chdir'ing the engine (n2::graph::set_cwd threads it as
        # a logical cwd). The output lands under subdir/.
        sub = work / "nsub"
        shutil.rmtree(sub, ignore_errors=True)
        sub.mkdir(parents=True, exist_ok=True)
        (sub / "build.ninja").write_text(
            "rule e\n  command = echo subninjaok > $out\n"
            "build sout.txt: e\n")
        r = run_ninja("NINJA10", work, "-C", "nsub")
        check(r.returncode == 0,
              f"case10: ninja -C box exits 0 (got {r.returncode}: {r.stderr[-600:]})")
        sp10 = latest_sqlar(m)
        rel10 = str((sub / "sout.txt").resolve()).lstrip("/")
        check(m.sqlar_content(sp10, rel10) == b"subninjaok\n",
              f"case10: ninja -C built subdir output via logical cwd "
              f"({m.sqlar_content(sp10, rel10)!r})")
        check(count_basename(sp10, "ninja") == 0,
              f"case10: no `ninja` process row — ran via the in-process builtin "
              f"(count={count_basename(sp10, 'ninja')})")

        # ── CASE 11: parallel recursive sub-makes share the process safely ──
        # `make -j2` runs the two recursive $(MAKE)s CONCURRENTLY (kati's parallel
        # scheduler dispatches the `a` and `b` recipes to two worker threads,
        # bounded by the slip pool). Each sub-make builds in its own subdir at the
        # right cwd (the recipe cwd is threaded explicitly to the worker). This
        # exercises the de-globalization for real: two Evaluators on two threads
        # must not clobber each other's per-instance state, and the reentrant
        # recipe runner must not deadlock. Both outputs appear; no `make` row;
        # engine lives.
        for d, txt in (("p1", "one"), ("p2", "two")):
            pd = work / d
            shutil.rmtree(pd, ignore_errors=True)
            pd.mkdir(parents=True, exist_ok=True)
            (pd / "Makefile").write_text(
                f"o.txt:\n\techo {txt} > $@\n")
        (work / "Makefile").write_text(
            "all: a b\n"
            "a:\n\t$(MAKE) -C p1\n"
            "b:\n\t$(MAKE) -C p2\n")
        r = run_make("MAKE11", work, "-j2")
        check(r.returncode == 0,
              f"case11: two sub-makes exit 0 (got {r.returncode}: {r.stderr[-600:]})")
        sp11 = latest_sqlar(m)
        rel_a = str((work / "p1" / "o.txt").resolve()).lstrip("/")
        rel_b = str((work / "p2" / "o.txt").resolve()).lstrip("/")
        check(m.sqlar_content(sp11, rel_a) == b"one\n",
              f"case11: recursive sub-make p1 built ({m.sqlar_content(sp11, rel_a)!r})")
        check(m.sqlar_content(sp11, rel_b) == b"two\n",
              f"case11: recursive sub-make p2 built ({m.sqlar_content(sp11, rel_b)!r})")
        check(count_basename(sp11, "make") == 0,
              f"case11: both sub-makes stayed in-process, no state clobber "
              f"(make rows={count_basename(sp11, 'make')})")

        # ── CASE 12: ninja -j2 runs recipes IN PARALLEL, bounded by the pool ──
        # The jobserver makes embedded n2 actually concurrent (the forced -j1 is
        # lifted under a pool) AND caps it: with -j2 over 4 targets that each
        # stamp their own start/end wall-clock, the max overlap must be exactly 2
        # — proving real parallelism (>1) and that the N-token pool bounds it
        # (not >2). Each recipe writes its OWN .start/.end files (no shared state,
        # so the measurement itself can't race).
        js = work / "js"
        shutil.rmtree(js, ignore_errors=True)
        js.mkdir(parents=True, exist_ok=True)
        rule = ("rule slow\n"
                "  command = date +%s.%N > $out.start ; sleep 0.4 ; "
                "date +%s.%N > $out.end ; echo ok > $out\n")
        builds = "".join(f"build t{i}.o: slow\n" for i in range(4))
        (js / "build.ninja").write_text(rule + builds)
        r = run_ninja("NINJA12", js, "-j2")
        check(r.returncode == 0,
              f"case12: ninja -j2 box exits 0 (got {r.returncode}: {r.stderr[-600:]})")
        sp12 = latest_sqlar(m)

        def stamp(name):
            rel = str((js / name).resolve()).lstrip("/")
            c = m.sqlar_content(sp12, rel)
            return float(c.strip()) if c else None

        intervals = []
        for i in range(4):
            s, e = stamp(f"t{i}.o.start"), stamp(f"t{i}.o.end")
            if s is not None and e is not None:
                intervals.append((s, e))
        # Max concurrency = peak number of intervals overlapping at any instant.
        events = sorted([(s, 1) for s, _ in intervals] + [(e, -1) for _, e in intervals])
        cur = peak = 0
        for _, d in events:
            cur += d
            peak = max(peak, cur)
        check(len(intervals) == 4,
              f"case12: all 4 recipes stamped start+end (got {len(intervals)})")
        check(peak >= 2,
              f"case12: recipes ran IN PARALLEL — embedded n2 -j1 is lifted "
              f"(peak concurrency={peak})")
        check(peak <= 2,
              f"case12: parallelism BOUNDED by the -j2 jobserver pool "
              f"(peak concurrency={peak}, must be <= 2)")
        check(count_basename(sp12, "ninja") == 0,
              f"case12: still in-process (ninja rows={count_basename(sp12, 'ninja')})")

        # ── CASE 13: the jobserver is advertised to recipes via MAKEFLAGS ──────
        # A parallel `make -j2` must export a jobserver in MAKEFLAGS so that
        # external consumers a recipe forks (e.g. `gcc -flto=jobserver`) draw from
        # the SAME pool. Capture what a recipe sees in $MAKEFLAGS and assert the
        # jobserver auth token is present.
        (work / "mf.mk").write_text(
            "all:\n\techo \"[$(MAKEFLAGS)]\" > mf.out\n")
        r = run_make("MAKE13", work, "-j2", "-f", "mf.mk")
        check(r.returncode == 0,
              f"case13: make -j2 box exits 0 (got {r.returncode}: {r.stderr[-600:]})")
        sp13 = latest_sqlar(m)
        mf = m.sqlar_content(sp13, str((work / "mf.out").resolve()).lstrip("/"))
        mf = mf.decode() if mf else ""
        check("--jobserver-auth=" in mf or "--jobserver-fds=" in mf,
              f"case13: recipe MAKEFLAGS carries the jobserver (mf={mf!r})")

        # ── CASE 14: the engine-global slip pool is reachable as a FUSE file ───
        # A box reads the synthetic /.slopbox-jobserver: the read() is a FUSE op
        # the engine handles as a slip ACQUIRE, handing back one token byte from
        # the machine-wide pool (slippool). This proves the FUSE-mediated pool is
        # live end-to-end (lookup → open → read=acquire) with per-op caller-pid
        # mediation — the foundation external `gcc -flto` and the in-process
        # schedulers will all draw from. (Pool is CPU-count by default, so the
        # first acquire always succeeds; bound/reaping behaviour is unit-tested in
        # slippool and exercised under real concurrency in the client increment.)
        r = subprocess.run(
            [str(BIN), "run", "-b", "JS14", "-C", str(work),
             "--", "head", "-c", "1", "/.slopbox-jobserver"],
            capture_output=True, text=True, timeout=60)
        check(r.returncode == 0,
              f"case14: jobserver read box exits 0 (got {r.returncode}: {r.stderr[-400:]})")
        check("+" in r.stdout,
              f"case14: read() of the FUSE jobserver acquired a slip byte "
              f"(stdout={r.stdout!r})")

        # ── CASE 15: a dead holder's leaked slip is REAPED ─────────────────────
        # The pool is pinned to 1 slip. Box A acquires it (read 1 byte) and exits
        # WITHOUT writing it back — a leak that a raw GNU-make pipe could never
        # recover. The engine watches the holder pid with a pidfd; on its exit the
        # slip returns to the pool. Box B then acquires the SAME (only) slip: if it
        # gets a byte, the leak was reaped; if reaping failed the pool is empty and
        # B's blocking read would hang (caught by the timeout).
        a = subprocess.run(
            [str(BIN), "run", "-b", "JSA15", "-C", str(work),
             "--", "head", "-c", "1", "/.slopbox-jobserver"],
            capture_output=True, text=True, timeout=60)
        check("+" in a.stdout,
              f"case15: box A acquired the only slip (stdout={a.stdout!r})")
        # Box B can only succeed if A's leaked slip was reclaimed on A's exit.
        b = subprocess.run(
            [str(BIN), "run", "-b", "JSB15", "-C", str(work),
             "--", "head", "-c", "1", "/.slopbox-jobserver"],
            capture_output=True, text=True, timeout=60)
        check("+" in b.stdout,
              f"case15: box B re-acquired the slip → A's leak was reaped "
              f"(stdout={b.stdout!r}, rc={b.returncode})")

        # ── CASE 16: `make -j2` runs recipes IN PARALLEL, bounded by the pool ──
        # The make analogue of case 12: kati's scheduler dispatches the two
        # independent targets' recipes to worker threads. With -j2 over two
        # timestamping targets the max overlap must be exactly 2 — real
        # parallelism (>1), bounded by n2's implicit token + the 1-slip pool.
        mj = work / "mj"
        shutil.rmtree(mj, ignore_errors=True)
        mj.mkdir(parents=True, exist_ok=True)
        (mj / "Makefile").write_text(
            "all: a b\n"
            "a:\n\tdate +%s.%N > a.start ; sleep 0.4 ; date +%s.%N > a.end\n"
            "b:\n\tdate +%s.%N > b.start ; sleep 0.4 ; date +%s.%N > b.end\n")
        r = run_make("MAKE16", mj, "-j2")
        check(r.returncode == 0,
              f"case16: make -j2 box exits 0 (got {r.returncode}: {r.stderr[-600:]})")
        sp16 = latest_sqlar(m)

        def mstamp(name):
            c = m.sqlar_content(sp16, str((mj / name).resolve()).lstrip("/"))
            return float(c.strip()) if c else None
        ivals = []
        for t in ("a", "b"):
            s, e = mstamp(f"{t}.start"), mstamp(f"{t}.end")
            if s is not None and e is not None:
                ivals.append((s, e))
        evs = sorted([(s, 1) for s, _ in ivals] + [(e, -1) for _, e in ivals])
        cur = peak = 0
        for _, d in evs:
            cur += d
            peak = max(peak, cur)
        check(len(ivals) == 2 and peak == 2,
              f"case16: make recipes ran in parallel, bounded (intervals={len(ivals)} "
              f"peak={peak}, want 2)")
        check(count_basename(sp16, "make") == 0,
              f"case16: parallel make stayed in-process (make rows={count_basename(sp16,'make')})")

        # ── CASE 17: dependency ordering is respected under -j ─────────────────
        # `final` depends on `dep`; final's recipe READS the file dep's recipe
        # writes. Even at -j4 the scheduler must run dep to completion before
        # final, so final sees dep's output (no race).
        od = work / "ord"
        shutil.rmtree(od, ignore_errors=True)
        od.mkdir(parents=True, exist_ok=True)
        (od / "Makefile").write_text(
            "final: dep\n\tcat dep.out > final.out\n"
            "dep:\n\techo depdata > dep.out\n")
        r = run_make("MAKE17", od, "-j4", "final")
        check(r.returncode == 0,
              f"case17: ordered make exits 0 (got {r.returncode}: {r.stderr[-600:]})")
        sp17 = latest_sqlar(m)
        fout = m.sqlar_content(sp17, str((od / "final.out").resolve()).lstrip("/"))
        check(fout == b"depdata\n",
              f"case17: final ran after its prereq dep under -j (final.out={fout!r})")

        # ── CASE 18: a later in-process make sees a file an earlier one made ──
        # Every $(MAKE) in a box runs in ONE shared engine process, so kati's
        # process-global glob/file caches outlive each invocation. The first make
        # here `-include cfg.mk` while it's ABSENT (poisoning the glob cache with
        # "missing"), then its recipe GENERATES cfg.mk. A second make must re-see
        # cfg.mk and pick up VAL — exactly busybox's `make defconfig` (writes
        # .config) then `make` (reads it). Without clearing the caches at each
        # make's entry, the second make reads the stale "missing" and VAL is
        # empty (busybox: empty lib.a archives → link fails). Both makes run in
        # ONE box (one process) via a single `sh -c`.
        cc = work / "cache"
        shutil.rmtree(cc, ignore_errors=True)
        cc.mkdir(parents=True, exist_ok=True)
        (cc / "gen.mk").write_text(
            "-include cfg.mk\n"
            "all:\n\t@echo 'VAL := yes' > cfg.mk\n")
        (cc / "use.mk").write_text(
            "-include cfg.mk\n"
            "all:\n\t@echo \"VAL=[$(VAL)]\" > result.txt\n")
        r = subprocess.run(
            [str(BIN), "run", "-b", "MAKE18", "-C", str(cc), "--",
             "sh", "-c", "make -f gen.mk && make -f use.mk"],
            capture_output=True, text=True, timeout=180)
        check(r.returncode == 0,
              f"case18: two-make box exits 0 (got {r.returncode}: {r.stderr[-600:]})")
        sp18 = latest_sqlar(m)
        res = m.sqlar_content(sp18, str((cc / "result.txt").resolve()).lstrip("/"))
        check(res == b"VAL=[yes]\n",
              f"case18: 2nd make saw the file the 1st made (no stale glob cache); "
              f"result.txt={res!r} (want b'VAL=[yes]\\n'; stale cache → b'VAL=[]\\n')")

        # ── CASE 19: `export` reaches a recursive sub-make AND its recipe env ──
        # A parent `export FOO` must reach a recursive $(MAKE): both as a make
        # variable ($(FOO)) and in the sub-make's RECIPE shell environment
        # ($$FOO). In a box many makes share one engine process, so exports can't
        # go through std::env (a data race + cross-make leak); they ride each
        # recipe's brush subshell via a non-echoed export prefix instead. This
        # checks BOTH halves — and that an UN-exported var does NOT leak ($$BAR
        # empty). Pre-env-fix the recipe shell env was empty (env=[]).
        ex = work / "exp"
        shutil.rmtree(ex, ignore_errors=True)
        ex.mkdir(parents=True, exist_ok=True)
        (ex / "Makefile").write_text(
            "export FOO := exported_val\n"
            "BAR := private_val\n"
            "all:\n\t@$(MAKE) -f sub.mk\n")
        (ex / "sub.mk").write_text(
            "all:\n\t@echo \"var=[$(FOO)] env=[$$FOO] bar_env=[$$BAR]\" > out.txt\n")
        r = subprocess.run(
            [str(BIN), "run", "-b", "MAKE19", "-C", str(ex), "--", "make"],
            capture_output=True, text=True, timeout=180)
        check(r.returncode == 0,
              f"case19: export box exits 0 (got {r.returncode}: {r.stderr[-600:]})")
        sp19 = latest_sqlar(m)
        eo = m.sqlar_content(sp19, str((ex / "out.txt").resolve()).lstrip("/"))
        check(eo == b"var=[exported_val] env=[exported_val] bar_env=[]\n",
              f"case19: export reached the sub-make as a var AND its recipe env, "
              f"and the non-exported var did NOT leak; out.txt={eo!r} "
              f"(want b'var=[exported_val] env=[exported_val] bar_env=[]\\n')")

        # ── CASE 20: MAKEFLAGS-appended flags reach a recursive sub-make ──────
        # The Linux kernel's top Makefile (mini-replica here, the v5.6 shape)
        # detects srctree != objtree, does `MAKEFLAGS += --include-dir=$(srctree)`
        # + `export sub_make_done := 1`, and re-invokes itself; the second pass
        # relies on the ENVIRONMENT MAKEFLAGS carrying --include-dir so its bare
        # `include scripts/Kbuild.include` resolves from the source tree. In a
        # box every $(MAKE) is an in-process builtin sharing ONE process env, so
        # MAKEFLAGS must ride the per-recipe export prefix (and be re-parsed
        # from the sub-make's seed env). Pre-fix the second pass died with
        # "scripts/Kbuild.include: No such file or directory" (the user-visible
        # kernel-build failure).
        kb = work / "kbuild"
        shutil.rmtree(kb, ignore_errors=True)
        (kb / "linux/scripts").mkdir(parents=True, exist_ok=True)
        (kb / "Makefile").write_text("all:\n\t$(MAKE) -f linux/Makefile foo\n")
        (kb / "linux/Makefile").write_text(
            "ifneq ($(sub_make_done),1)\n"
            "MAKEFLAGS += -rR\n"
            "abs_objtree := $(CURDIR)\n"
            "abs_srctree := $(realpath $(dir $(lastword $(MAKEFILE_LIST))))\n"
            "ifneq ($(abs_srctree),$(abs_objtree))\n"
            "MAKEFLAGS += --include-dir=$(abs_srctree)\n"
            "need-sub-make := 1\n"
            "endif\n"
            "export abs_srctree abs_objtree\n"
            "export sub_make_done := 1\n"
            "ifeq ($(need-sub-make),1)\n"
            "$(filter-out _all sub-make $(lastword $(MAKEFILE_LIST)), "
            "$(MAKECMDGOALS)) _all: sub-make\n"
            "\t@:\n"
            "sub-make:\n"
            "\t$(Q)$(MAKE) -C $(abs_objtree) -f $(abs_srctree)/Makefile "
            "$(MAKECMDGOALS)\n"
            "endif\n"
            "endif\n"
            "ifeq ($(need-sub-make),)\n"
            "include scripts/Kbuild.include\n"
            "foo:\n"
            "\t@echo built-foo mark=$(KBUILD_MARK) > $(abs_srctree)/../kb.txt\n"
            "endif\n")
        (kb / "linux/scripts/Kbuild.include").write_text("KBUILD_MARK := yes\n")
        r = subprocess.run(
            [str(BIN), "run", "-b", "MAKE20", "-C", str(kb), "--", "make"],
            capture_output=True, text=True, timeout=180)
        check(r.returncode == 0,
              f"case20: kbuild sub-make box exits 0 (got {r.returncode}: "
              f"{r.stderr[-600:]})")
        sp20 = latest_sqlar(m)
        ko = m.sqlar_content(sp20, str((kb / "kb.txt").resolve()).lstrip("/"))
        check(ko == b"built-foo mark=yes\n",
              f"case20: second pass found scripts/Kbuild.include via the "
              f"MAKEFLAGS-inherited --include-dir; kb.txt={ko!r} "
              f"(want b'built-foo mark=yes\\n')")

        # ── CASE 21: VPATH source resolution + pattern-rule remade include ────
        # Two kernel-build essentials in one makefile: (a) a REQUIRED
        # `include gen.conf` whose file doesn't exist at parse time and is
        # produced by a PATTERN rule (`%.conf:`) — GNU's remake-the-makefile
        # loop must accept pattern-rule producers (the kernel's
        # `%/auto.conf: $(KCONFIG_CONFIG)` → syncconfig); (b) `VPATH := sub`
        # resolving the prerequisite `src.c` to sub/src.c and rewriting $< to
        # the found path (the kernel's out-of-tree `VPATH := $(srctree)`).
        vp = work / "vpath"
        shutil.rmtree(vp, ignore_errors=True)
        (vp / "sub").mkdir(parents=True, exist_ok=True)
        (vp / "sub/src.c").write_text("payload-from-vpath\n")
        (vp / "Makefile").write_text(
            "VPATH := sub\n"
            "include gen.conf\n"
            "all: out.txt\n"
            "\t@echo conf=$(CONF) > conf.txt\n"
            "out.txt: src.c\n"
            "\t@cp $< $@\n"
            "%.conf:\n"
            "\t@echo 'CONF := yes' > $@\n")
        r = subprocess.run(
            [str(BIN), "run", "-b", "MAKE21", "-C", str(vp), "--", "make"],
            capture_output=True, text=True, timeout=180)
        check(r.returncode == 0,
              f"case21: vpath box exits 0 (got {r.returncode}: {r.stderr[-600:]})")
        sp21 = latest_sqlar(m)
        vo = m.sqlar_content(sp21, str((vp / "out.txt").resolve()).lstrip("/"))
        co = m.sqlar_content(sp21, str((vp / "conf.txt").resolve()).lstrip("/"))
        check(vo == b"payload-from-vpath\n",
              f"case21: $< resolved through VPATH to sub/src.c; out.txt={vo!r}")
        check(co == b"conf=yes\n",
              f"case21: required include regenerated via the %.conf pattern "
              f"rule (remake loop); conf.txt={co!r}")

        # ── CASE 22: a recipe's `set -e` must NOT leak into a nested sh ───────
        # kbuild's cmd macro runs every recipe under `set -e`, then invokes
        # `$(CONFIG_SHELL) scripts/headers_install.sh …`, which deliberately
        # lets unifdef exit 1 (output-changed) and inspects $?. The in-process
        # snooped `sh script.sh` cloned the caller's shell WITH its errexit, so
        # the script died at unifdef's benign exit 1 and `make headers` failed
        # on the first header unifdef modified. A nested sh is a fresh shell:
        # default options, only its own argv flags apply.
        ee = work / "errexit"
        shutil.rmtree(ee, ignore_errors=True)
        ee.mkdir(parents=True, exist_ok=True)
        (ee / "script.sh").write_text(
            "false\n"
            "[ $? -gt 1 ] && exit 1\n"
            "echo script-ran > result.txt\n")
        (ee / "Makefile").write_text(
            "all:\n\t@set -e; sh script.sh; echo recipe-done >> result.txt\n")
        r = subprocess.run(
            [str(BIN), "run", "-b", "MAKE22", "-C", str(ee), "--", "make"],
            capture_output=True, text=True, timeout=180)
        check(r.returncode == 0,
              f"case22: errexit box exits 0 (got {r.returncode}: {r.stderr[-600:]})")
        sp22 = latest_sqlar(m)
        eo22 = m.sqlar_content(sp22, str((ee / "result.txt").resolve()).lstrip("/"))
        check(eo22 == b"script-ran\nrecipe-done\n",
              f"case22: nested sh ran with fresh options under a set -e recipe; "
              f"result.txt={eo22!r} (want b'script-ran\\nrecipe-done\\n')")

        # ── CASE 23: order-only prereq existence probes the make's OWN dir ────
        # The scheduler skips an order-only prerequisite that already exists —
        # but it probed the PROCESS cwd, not the make's logical working dir.
        # In a box every in-process `make -C sub` keeps the engine's cwd, so a
        # SAME-NAMED file in the box's start dir made the sub-make's order-only
        # prereq look already-built and it was silently never generated; its
        # consumer then failed ("not generating needed file" — immediately
        # under -j, serially whenever no other edge built it). The decoy
        # gen.txt below sits in the START dir; the real one must be built in
        # sub/.
        oo = work / "oo"
        shutil.rmtree(oo, ignore_errors=True)
        (oo / "sub").mkdir(parents=True, exist_ok=True)
        (oo / "gen.txt").write_text("decoy-from-parent\n")
        (oo / "Makefile").write_text("all:\n\t@$(MAKE) -C sub\n")
        (oo / "sub/Makefile").write_text(
            "all: out.txt\n"
            "out.txt: | gen.txt\n"
            "\t@cat gen.txt > out.txt\n"
            "gen.txt:\n"
            "\t@echo made-in-sub > gen.txt\n")
        r = subprocess.run(
            [str(BIN), "run", "-b", "MAKE23", "-C", str(oo), "--", "make"],
            capture_output=True, text=True, timeout=180)
        check(r.returncode == 0,
              f"case23: order-only box exits 0 (got {r.returncode}: "
              f"{r.stderr[-600:]})")
        sp23 = latest_sqlar(m)
        oo23 = m.sqlar_content(sp23, str((oo / "sub/out.txt").resolve()).lstrip("/"))
        check(oo23 == b"made-in-sub\n",
              f"case23: order-only prereq built in the sub-make's OWN dir "
              f"(not skipped for the parent's decoy); out.txt={oo23!r}")

        # ── CASE 24: variable provenance lands in the box makevar table ───────
        # OPT-IN via `run --vars`. Every assignment a traced build makes —
        # make (:=, +=, with file:line loc), shell scalar inside a recipe,
        # `export NAME=…` via the export builtin, and a sub-make's own
        # override — must be recorded in the box DB's makevar table with the
        # UNEXPANDED rhs, the variable names it dereferences, and its
        # execution context (recipe edge / pipeline uid), so the Vars pane
        # can search, walk the chain, and cross-navigate. Export-prefix noise
        # (box exports replayed per subshell) must NOT appear; a box run
        # WITHOUT --vars must record nothing.
        vt = work / "vars"
        shutil.rmtree(vt, ignore_errors=True)
        vt.mkdir(parents=True, exist_ok=True)
        (vt / "Makefile").write_text(
            "ORIG_VAR := aa\n"
            "ORIG_VAR += bb\n"
            "DERIVED := pre-$(ORIG_VAR)\n"
            "all:\n"
            "\t@SHELLVAR=\"from-shell $(ORIG_VAR)\"; "
            "export EXPVAR=\"exported $$SHELLVAR\"; "
            "$(MAKE) -f sub.mk\n")
        (vt / "sub.mk").write_text(
            "ORIG_VAR := sub-val\n"
            "all:\n\t@true\n")
        r = subprocess.run(
            [str(BIN), "run", "-b", "--vars", "MAKE24", "-C", str(vt),
             "--", "make"],
            capture_output=True, text=True, timeout=180)
        check(r.returncode == 0,
              f"case24: vars box exits 0 (got {r.returncode}: {r.stderr[-600:]})")
        sp24 = latest_sqlar(m)
        con = sqlite3.connect(f"file:{sp24}?mode=ro", uri=True)
        try:
            mv = list(con.execute(
                "SELECT name, loc, value, make_dir, rhs, refs, edge_out, uid, "
                "flags FROM makevar ORDER BY id"))
        finally:
            con.close()
        def mv_row(name, loc_sub, value, make_sub):
            for row in mv:
                if (row[0] == name and loc_sub in (row[1] or "")
                        and row[2] == value and make_sub in (row[3] or "")):
                    return row
            return None
        check(mv_row("ORIG_VAR", "Makefile:1", "aa", "vars") is not None,
              f"case24: make := assignment recorded with file:line loc ({mv})")
        check(mv_row("ORIG_VAR", "Makefile:2", "aa bb", "vars") is not None,
              f"case24: make += records the appended value")
        drow = mv_row("DERIVED", "Makefile:3", "pre-aa bb", "vars")
        check(drow is not None and drow[4] == "pre-$(ORIG_VAR)"
              and drow[5] == "ORIG_VAR",
              f"case24: DERIVED keeps the unexpanded rhs + its dereference "
              f"(rhs={drow[4] if drow else None!r} "
              f"refs={drow[5] if drow else None!r})")
        srow = mv_row("SHELLVAR", "recipe of all", "from-shell aa bb", "sh")
        check(srow is not None,
              f"case24: shell scalar inside the recipe recorded, expanded")
        check(srow is not None and srow[6] == "all",
              f"case24: shell assignment anchored to its recipe edge "
              f"(edge_out={srow[6] if srow else None!r})")
        check(mv_row("EXPVAR", "recipe of all", "exported from-shell aa bb",
                     "sh export") is not None,
              f"case24: `export NAME=…` (builtin path) recorded as sh export")
        subrow = mv_row("ORIG_VAR", "sub.mk:1", "sub-val", "vars")
        check(subrow is not None,
              f"case24: sub-make's own assignment recorded with sub.mk loc")
        check(subrow is not None and subrow[6] == "all",
              f"case24: sub-make parse anchored to the spawning recipe edge "
              f"(edge_out={subrow[6] if subrow else None!r})")
        check(not any("PATH" == row[0] for row in mv),
              f"case24: export-prefix replay is suppressed (no PATH rows)")
        approw = mv_row("ORIG_VAR", "Makefile:2", "aa bb", "vars")
        exprow = mv_row("EXPVAR", "recipe of all",
                        "exported from-shell aa bb", "sh export")
        check(approw is not None and approw[8] == "+=",
              f"case24: += flagged as append "
              f"(flags={approw[8] if approw else None!r})")
        check(srow is not None and srow[8] == "sh",
              f"case24: shell scalar flagged sh "
              f"(flags={srow[8] if srow else None!r})")
        check(exprow is not None and exprow[8] == "sh x",
              f"case24: export-builtin assignment flagged 'sh x' "
              f"(flags={exprow[8] if exprow else None!r})")
        # Same build WITHOUT --vars: silence.
        r = subprocess.run(
            [str(BIN), "run", "-b", "MAKE24B", "-C", str(vt), "--", "make"],
            capture_output=True, text=True, timeout=180)
        check(r.returncode == 0,
              f"case24: untraced box exits 0 (got {r.returncode})")
        sp24b = latest_sqlar(m)
        con = sqlite3.connect(f"file:{sp24b}?mode=ro", uri=True)
        try:
            n_off = con.execute("SELECT count(*) FROM makevar").fetchone()[0]
        finally:
            con.close()
        check(n_off == 0,
              f"case24: variable tracing is OFF by default ({n_off} rows "
              f"recorded without --vars)")
        # ── CASE 25: command-line vars propagate to sub-makes (MAKEFLAGS --) ──
        # `make install DESTDIR=/x` must mean DESTDIR=/x in EVERY sub-make —
        # GNU carries command-line overrides in MAKEFLAGS after a `--`
        # separator, space-escaped. Losing them un-prefixed every install
        # path ("install: cannot create directory"). Values with spaces must
        # survive the round-trip and keep command-line origin (beating ?=).
        cv = work / "clvars"
        shutil.rmtree(cv, ignore_errors=True)
        (cv / "sub").mkdir(parents=True, exist_ok=True)
        (cv / "Makefile").write_text("all:\n\t@$(MAKE) -s -C sub show\n")
        (cv / "sub/Makefile").write_text(
            "DESTDIR ?= lost\n"
            "show:\n"
            "\t@echo \"D=[$(DESTDIR)] V=[$(V)] o=[$(origin DESTDIR)]\" "
            "> $(RESULT)\n")
        r = subprocess.run(
            [str(BIN), "run", "-b", "MAKE25", "-C", str(cv), "--",
             "make", "DESTDIR=/pfx", "V=aa bb", f"RESULT={cv}/out.txt"],
            capture_output=True, text=True, timeout=180)
        check(r.returncode == 0,
              f"case25: cl-vars box exits 0 (got {r.returncode}: "
              f"{r.stderr[-600:]})")
        sp25 = latest_sqlar(m)
        cvo = m.sqlar_content(sp25, str((cv / "out.txt").resolve()).lstrip("/"))
        check(cvo == b"D=[/pfx] V=[aa bb] o=[command line]\n",
              f"case25: command-line vars (incl. a spaced value) reach the "
              f"sub-make with command-line origin; got {cvo!r}")
        # ── CASE 26: `make -f -` — makefile on stdin ──────────────────────────
        # automake's dependency-tracking bootstrap pipes a sed-filtered
        # Makefile through `$MAKE -f - am--depfiles`; without stdin-makefile
        # support every autoconf-generated config.status fails.
        sm = work / "stdinmk"
        shutil.rmtree(sm, ignore_errors=True)
        sm.mkdir(parents=True, exist_ok=True)
        r = subprocess.run(
            [str(BIN), "run", "-b", "MAKE26", "-C", str(sm), "--", "sh", "-c",
             'printf "x:\n\t@echo stdin-mk > result.txt\n" | make -f - x'],
            capture_output=True, text=True, timeout=180)
        check(r.returncode == 0,
              f"case26: stdin-makefile box exits 0 (got {r.returncode}: "
              f"{r.stderr[-400:]})")
        sp26 = latest_sqlar(m)
        so = m.sqlar_content(sp26, str((sm / "result.txt").resolve()).lstrip("/"))
        check(so == b"stdin-mk\n",
              f"case26: `make -f -` built from the piped makefile; got {so!r}")
        # ── CASE 27: Linux's GNU-make-4 capability gate + top-level errexit ─
        # Linux rejects make engines whose .FEATURES lacks output-sync. Also
        # exercise `set -e` in the TOP `run -b` program (not merely a nested
        # recipe/script): provenance executes complete commands separately, but
        # that split must preserve ExitShell and skip everything after false.
        k4 = work / "make4"
        shutil.rmtree(k4, ignore_errors=True)
        k4.mkdir(parents=True, exist_ok=True)
        (k4 / "Makefile").write_text(
            "ifeq ($(filter output-sync,$(.FEATURES)),)\n"
            "$(error output-sync missing)\n"
            "endif\n"
            "all:\n\t@echo feature-ok > feature.txt\n")
        r = subprocess.run(
            [str(BIN), "run", "-b", "MAKE27", "-C", str(k4), "--",
             "sh", "-c", "set -e; make; false; echo escaped > escaped.txt"],
            capture_output=True, text=True, timeout=180)
        check(r.returncode == 1,
              f"case27: top-level errexit returns the failing status "
              f"(got {r.returncode}: {r.stderr[-400:]})")
        sp27 = latest_sqlar(m)
        feature = m.sqlar_content(
            sp27, str((k4 / "feature.txt").resolve()).lstrip("/"))
        escaped = m.sqlar_content(
            sp27, str((k4 / "escaped.txt").resolve()).lstrip("/"))
        check(feature == b"feature-ok\n",
              f"case27: embedded make passes Linux's output-sync feature gate; "
              f"got {feature!r}")
        check(escaped is None,
              f"case27: command after top-level `set -e; false` was skipped; "
              f"escaped={escaped!r}")
        # ── CASE 28: compiler discovery through an in-process shebang child ─
        # Linux selects scripts/Makefile.clang by inspecting `$(CC) --version`,
        # appends CLANG_FLAGS in the included fragment, exports them, and then
        # consumes them from a recursive make.  Keep that complete shape here:
        # a shebang wrapper's stdout and argv must survive $(shell), and its
        # resulting make variable must survive the sub-make boundary.
        kd = work / "compiler-discovery"
        shutil.rmtree(kd, ignore_errors=True)
        kd.mkdir(parents=True, exist_ok=True)
        wrapper = kd / "clang-probe"
        wrapper.write_text(
            "#!/bin/sh\n"
            "set -u\n"
            "state=${SARUN_CLANG_PROBE_STATE:?}\n"
            "IFS=' ' read -r started _ < /proc/uptime\n"
            "trace=\"$state/trace.$$.$started\"\n"
            "printf '%s\\n' \"$started\" > \"$trace\"\n"
            "/usr/bin/clang-21 \"$@\"\n"
            "status=$?\n"
            "IFS=' ' read -r ended _ < /proc/uptime\n"
            "printf '%s\\n' \"$ended\" >> \"$trace\"\n"
            "exit \"$status\"\n")
        wrapper.chmod(0o755)
        (kd / "traces").mkdir()
        (kd / "clang.mk").write_text(
            "CLANG_FLAGS += --target=aarch64-linux-gnu\n"
            "CLANG_FLAGS += -fintegrated-as\n"
            "export CLANG_FLAGS\n")
        (kd / "child.mk").write_text(
            "all:\n\t@printf '%s|%s\\n' \"$(CLANG_FLAGS)\" "
            "\"$(SARUN_CLANG_PROBE_STATE)\" > result.txt\n")
        (kd / "Makefile").write_text(
            "CC := ./clang-probe\n"
            f"export SARUN_CLANG_PROBE_STATE := {kd}/traces\n"
            "CLANG_FLAGS :=\n"
            "CC_VERSION_TEXT = $(shell $(CC) --version 2>/dev/null | head -n 1)\n"
            "ifneq ($(findstring clang,$(CC_VERSION_TEXT)),)\n"
            "include clang.mk\n"
            "endif\n"
            "all:\n\t@$(MAKE) -f child.mk\n")
        r = run_make("MAKE28", kd, "-j10")
        check(r.returncode == 0,
              f"case28: compiler discovery/sub-make exits promptly and cleanly "
              f"(got {r.returncode}: {r.stderr[-600:]})")
        sp28 = latest_sqlar(m)
        discovered = m.sqlar_content(
            sp28, str((kd / "result.txt").resolve()).lstrip("/"))
        trace_names = [n for n in sqlar_names(sp28) if "traces/trace." in n]
        trace_data = [(n, m.sqlar_content(sp28, n)) for n in trace_names]
        expected_discovery = (
            b"--target=aarch64-linux-gnu -fintegrated-as|" +
            str(kd / "traces").encode() + b"\n")
        check(discovered == expected_discovery,
              f"case28: shebang compiler identity selects and exports clang "
              f"flags through recursive make; got {discovered!r}; "
              f"traces={trace_data!r}")
        check(len(trace_data) == 1 and trace_data[0][1] is not None
              and len(trace_data[0][1].splitlines()) == 2,
              f"case28: the exact compiler wrapper completed its start/end "
              f"trace around clang; traces={trace_data!r}")
        # ── CASE 29: parallel parent promptly reaps a failing sub-make ───
        # The kernel exposed a failure path where syncconfig's recursive make
        # returned non-zero while sibling jobs were active, but the -j10 parent
        # spun forever instead of draining workers and returning the failure.
        pf = work / "parallel-failure"
        shutil.rmtree(pf, ignore_errors=True)
        pf.mkdir(parents=True, exist_ok=True)
        (pf / "fail.mk").write_text("boom:\n\t@false\n")
        siblings = " ".join(f"s{i}" for i in range(9))
        (pf / "Makefile").write_text(
            f"all: recurse late {siblings}\n"
            "recurse:\n\t@$(MAKE) -f fail.mk boom\n" +
            "late: slow\n\t@printf late > $@\n"
            "slow:\n\t@sleep 0.2\n" +
            "".join(
                f"s{i}:\n\t@sleep 0.1; echo {i} > $@\n" for i in range(9)))
        r = subprocess.run(
            [str(BIN), "run", "-b", "MAKE29", "-C", str(pf), "--",
             "make", "-j10"],
            capture_output=True, text=True, timeout=20)
        check(r.returncode != 0,
              f"case29: failing recursive make under -j10 returns promptly "
              f"and non-zero (got {r.returncode}: {(r.stdout+r.stderr)[-600:]})")
        # ── CASE 30: Kbuild's expanded static-pattern subdirectory gate ──
        # scripts/Makefile.build turns each nested archive into a static-pattern
        # target whose stem-specific prerequisite is a phony subdirectory.  The
        # phony recipe recursively builds the archive; the empty static-pattern
        # rule is the ordering bridge consumed by the aggregate archive.
        sp = work / "static-pattern-subdirs"
        shutil.rmtree(sp, ignore_errors=True)
        sp.mkdir(parents=True, exist_ok=True)
        (sp / "sub.mk").write_text(
            "targets-for-builtin :=\n"
            "ifdef need-builtin\n"
            "targets-for-builtin += $(obj)/built-in.a\n"
            "endif\n"
            "$(obj)/: $(if $(KBUILD_BUILTIN),$(targets-for-builtin))\n"
            "\t@:\n"
            ".PHONY: FORCE\n"
            "$(obj)/built-in.a: FORCE\n"
            "\t@mkdir -p $(obj); printf '%s\\n' $(obj) > $@\n")
        (sp / "Makefile").write_text(
            "obj := fs\n"
            "export KBUILD_BUILTIN := y\n"
            "subdir-builtin := fs/notify/built-in.a fs/quota/built-in.a\n"
            ".PHONY: fs/notify fs/quota\n"
            "$(subdir-builtin): $(obj)/%/built-in.a: $(obj)/% ;\n"
            "fs/notify fs/quota:\n"
            "\t@$(MAKE) -f sub.mk obj=$@ need-builtin=1\n"
            "fs/built-in.a: $(subdir-builtin)\n"
            "\t@cat $^ > $@\n"
            "all: fs/built-in.a\n")
        r = run_make("MAKE30", sp, "-j10", "all")
        check(r.returncode == 0,
              f"case30: expanded static-pattern prerequisites order nested "
              f"archives under -j10 (got {r.returncode}: "
              f"{(r.stdout+r.stderr)[-800:]})")
        sp30 = latest_sqlar(m)
        combined = m.sqlar_content(
            sp30, str((sp / "fs/built-in.a").resolve()).lstrip("/"))
        check(combined == b"fs/notify\nfs/quota\n",
              f"case30: each concrete target substitutes its own stem into "
              f"the phony prerequisite; got {combined!r}")
        # ── CASE 31: inherited exports survive a second recursive level ──
        # Kbuild's top make exports KBUILD_BUILTIN, the fs make inherits it,
        # and the fs/notify make must inherit it AGAIN.  Environment-origin
        # names stay exported even when the intermediate makefile never says
        # `export` itself; losing that property makes the grandchild's default
        # directory goal omit its built-in archive while still returning 0.
        ml = work / "multilevel-export"
        shutil.rmtree(ml, ignore_errors=True)
        ml.mkdir(parents=True, exist_ok=True)
        (ml / "build.mk").write_text(
            "ifeq ($(obj),fs)\n"
            "obj-y := notify/\n"
            "endif\n"
            "ifdef need-builtin\n"
            "obj-y := $(patsubst %/,%/built-in.a,$(obj-y))\n"
            "endif\n"
            "real-obj-y := $(addprefix $(obj)/,$(obj-y))\n"
            "subdir-builtin := $(filter %/built-in.a,$(real-obj-y))\n"
            "targets-for-builtin := $(if $(need-builtin),$(obj)/built-in.a)\n"
            "$(subdir-builtin): $(obj)/%/built-in.a: $(obj)/% ;\n"
            ".PHONY: $(patsubst %/built-in.a,%,$(subdir-builtin)) FORCE\n"
            "$(patsubst %/built-in.a,%,$(subdir-builtin)):\n"
            "\t@$(MAKE) -f build.mk obj=$@ need-builtin=1\n"
            "$(obj)/built-in.a: $(real-obj-y) FORCE\n"
            "\t@mkdir -p $(obj); printf '%s\\n' $(obj) > $@\n"
            "$(obj)/: $(if $(KBUILD_BUILTIN),$(targets-for-builtin)) "
            "$(patsubst %/built-in.a,%,$(subdir-builtin))\n"
            "\t@:\n")
        (ml / "Makefile").write_text(
            "export KBUILD_BUILTIN := y\n"
            ".PHONY: fs\n"
            "fs/built-in.a: fs ;\n"
            "fs:\n\t@$(MAKE) -f build.mk obj=fs need-builtin=1\n"
            "all: fs/built-in.a\n")
        r = run_make("MAKE31", ml, "-j10", "all")
        check(r.returncode == 0,
              f"case31: a make-inherited export survives two recursive "
              f"make boundaries (got {r.returncode}: "
              f"{(r.stdout+r.stderr)[-800:]})")
        sp31 = latest_sqlar(m)
        grandchild = m.sqlar_content(
            sp31, str((ml / "fs/notify/built-in.a").resolve()).lstrip("/"))
        check(grandchild == b"fs/notify\n",
              f"case31: grandchild default goal includes built-in archive; "
              f"got {grandchild!r}")
        # ── CASE 32: conditional recipe continuation after an include ────
        cr = work / "conditional-recipe"
        shutil.rmtree(cr, ignore_errors=True)
        cr.mkdir(parents=True, exist_ok=True)
        (cr / "feature.mk").write_text("CONFIG_COMPAT_VDSO=y\n")
        (cr / "Makefile").write_text(
            "include feature.mk\n"
            "all:\n"
            "\t@printf one > result.txt\n"
            "ifdef CONFIG_COMPAT_VDSO\n"
            "\t@printf two >> result.txt\n"
            "endif\n")
        r = run_make("MAKE32", cr)
        check(r.returncode == 0,
              f"case32: conditional continuation of the preceding recipe "
              f"runs (got {r.returncode}: {(r.stdout+r.stderr)[-500:]})")
        sp32 = latest_sqlar(m)
        conditional = m.sqlar_content(
            sp32, str((cr / "result.txt").resolve()).lstrip("/"))
        check(conditional == b"onetwo",
              f"case32: included nonempty CONFIG enables the second recipe "
              f"line; got {conditional!r}")
        # ── CASE 33: an EXISTING stale include is remade and reparsed ────
        # Kbuild leaves include/config/auto.conf present after a config merge,
        # but makes .config newer. GNU make treats every parsed makefile as a
        # target, refreshes the stale include, and restarts before evaluating
        # CONFIG-dependent architecture recipes.
        sr = work / "stale-remade-include"
        shutil.rmtree(sr, ignore_errors=True)
        sr.mkdir(parents=True, exist_ok=True)
        (sr / "source.conf").write_text("VALUE := fresh\n")
        (sr / "generated.conf").write_text("VALUE := stale\n")
        now = time.time()
        os.utime(sr / "generated.conf", (now - 10, now - 10))
        os.utime(sr / "source.conf", (now, now))
        (sr / "Makefile").write_text(
            "include generated.conf\n"
            "all:\n"
            "\t@printf '%s\\n' '$(VALUE)' > result.txt\n"
            "generated.conf: source.conf\n"
            "\t@cp $< $@\n")
        r = run_make("MAKE33", sr)
        check(r.returncode == 0,
              f"case33: stale existing include is remade before main goals "
              f"(got {r.returncode}: {(r.stdout+r.stderr)[-500:]})")
        sp33 = latest_sqlar(m)
        remade = m.sqlar_content(
            sp33, str((sr / "result.txt").resolve()).lstrip("/"))
        check(remade == b"fresh\n",
              f"case33: main recipe sees reparsed generated include; "
              f"got {remade!r}")
        # ── CASE 34: shared and repeated prerequisites run each target once ─
        # A parallel dependency-count scheduler may encounter the same target
        # through several release paths. Once selected, that target is active:
        # a later zero-count observation must not prepare or launch it again.
        dq = work / "duplicate-ready"
        shutil.rmtree(dq, ignore_errors=True)
        dq.mkdir(parents=True, exist_ok=True)
        (dq / "Makefile").write_text(
            ".PHONY: all left right shared\n"
            "all: left right shared shared\n"
            "left: shared shared\n\t@printf x >> left.ran\n"
            "right: shared\n\t@printf x >> right.ran\n"
            "shared:\n\t@printf x >> shared.ran\n")
        r = run_make("MAKE34", dq, "-j10", "all")
        check(r.returncode == 0,
              f"case34: duplicate/shared dependency graph completes under "
              f"-j10 (got {r.returncode}: {(r.stdout+r.stderr)[-500:]})")
        sp34 = latest_sqlar(m)
        got = {
            name: m.sqlar_content(
                sp34, str((dq / f"{name}.ran").resolve()).lstrip("/"))
            for name in ("left", "right", "shared")
        }
        check(got == {"left": b"x", "right": b"x", "shared": b"x"},
              f"case34: every recipe runs exactly once; got {got!r}")
        # ── CASE 35: recipe expansion is bounded by execution capacity ────
        # GNU expands recipes when they are selected to run. In a -j1 build,
        # the second recipe therefore sees files created by the first. Draining
        # and expanding the whole ready graph before dispatch breaks that
        # semantic and starves workers on large Kbuild archive graphs.
        rexp = work / "recipe-expansion-order"
        shutil.rmtree(rexp, ignore_errors=True)
        rexp.mkdir(parents=True, exist_ok=True)
        (rexp / "Makefile").write_text(
            ".PHONY: all one two\n"
            "all: one two\n"
            "one:\n\t@printf done > marker\n"
            "two:\n"
            "\t@$(if $(wildcard marker),printf '%s\\n' observed > result.txt,false)\n")
        r = run_make("MAKE35", rexp, "-j1", "all")
        check(r.returncode == 0,
              f"case35: -j1 dispatches the first recipe before expanding the "
              f"second (got {r.returncode}: {(r.stdout+r.stderr)[-500:]})")
        sp35 = latest_sqlar(m)
        observed = m.sqlar_content(
            sp35, str((rexp / "result.txt").resolve()).lstrip("/"))
        check(observed == b"observed\n",
              f"case35: later recipe expansion observes earlier output; "
              f"got {observed!r}")
        # ── CASE 36: specific chained pattern beats generic chain ─────────
        # ARM64 Kbuild has `%.pi.o: %.o` alongside generic `%.o: %.S` and
        # `%.o: %.c`. GNU selects the target pattern yielding the shortest stem;
        # otherwise foo.pi.o is misread as an assembly object needing foo.pi.S.
        ps = work / "pattern-specificity"
        shutil.rmtree(ps, ignore_errors=True)
        ps.mkdir(parents=True, exist_ok=True)
        (ps / "unit.c").write_text("source\n")
        (ps / "Makefile").write_text(
            ".PHONY: all\n"
            "all: unit.pi.o\n"
            "%.pi.o: %.o\n\t@cp $< $@\n"
            "%.o: %.c\n\t@cp $< $@\n"
            "%.o: %.S\n\t@false\n"
            "%.S: %_shipped\n\t@cp $< $@\n")
        r = run_make("MAKE36", ps, "-j10", "all")
        check(r.returncode == 0,
              f"case36: specific %.pi.o chain beats generic %.o chain "
              f"(got {r.returncode}: {(r.stdout+r.stderr)[-500:]})")
        sp36 = latest_sqlar(m)
        piobj = m.sqlar_content(
            sp36, str((ps / "unit.pi.o").resolve()).lstrip("/"))
        check(piobj == b"source\n",
              f"case36: chained %.pi.o was built through existing unit.c; "
              f"got {piobj!r}")
        # ── CASE 37: overlapping pattern-specific += composes ─────────────
        # ARM64 Kbuild gives every %.pi.o common objcopy flags, then appends a
        # section flag for lib-%.pi.o. The independently stored pattern scopes
        # must compose for the concrete target rather than the specific value
        # replacing the common prefix-symbol flags.
        pv = work / "pattern-variable-compose"
        shutil.rmtree(pv, ignore_errors=True)
        pv.mkdir(parents=True, exist_ok=True)
        (pv / "lib-unit.c").write_text("source\n")
        (pv / "Makefile").write_text(
            ".PHONY: all\n"
            "all: lib-unit.pi.o\n"
            "%.pi.o: FLAGS := common\n"
            "lib-%.pi.o: FLAGS += specific\n"
            "%.pi.o: %.o\n\t@printf '%s\\n' '$(FLAGS)' > $@\n"
            "%.o: %.c\n\t@cp $< $@\n")
        r = run_make("MAKE37", pv, "-j10", "all")
        check(r.returncode == 0,
              f"case37: overlapping pattern-specific variables compose "
              f"(got {r.returncode}: {(r.stdout+r.stderr)[-500:]})")
        sp37 = latest_sqlar(m)
        flags = m.sqlar_content(
            sp37, str((pv / "lib-unit.pi.o").resolve()).lstrip("/"))
        check(flags == b"common specific\n",
              f"case37: specific += retains common pattern value; got {flags!r}")
        # ── CASE 38: .PHONY targets skip match-anything implicit rules ─────
        # OpenWrt uses a `%::` fallback to re-enter its real build after the
        # prerequisite pass and a phony FORCE target throughout the graph. GNU
        # never applies implicit rules to .PHONY targets; doing so makes FORCE
        # recursively invoke the fallback forever.
        ph = work / "phony-implicit-skip"
        shutil.rmtree(ph, ignore_errors=True)
        ph.mkdir(parents=True, exist_ok=True)
        (ph / "Makefile").write_text(
            ".PHONY: all FORCE\n"
            "all: FORCE\n\t@printf good > result.txt\n"
            "%::\n\t@printf bad >> fallback.txt\n")
        r = run_make("MAKE38", ph, "-j10", "all")
        check(r.returncode == 0,
              f"case38: phony FORCE completes without the %:: fallback "
              f"(got {r.returncode}: {(r.stdout+r.stderr)[-500:]})")
        sp38 = latest_sqlar(m)
        good = m.sqlar_content(
            sp38, str((ph / "result.txt").resolve()).lstrip("/"))
        bad = m.sqlar_content(
            sp38, str((ph / "fallback.txt").resolve()).lstrip("/"))
        check(good == b"good" and bad is None,
              f"case38: implicit search is skipped for .PHONY; "
              f"good={good!r} fallback={bad!r}")
        # ── CASE 39: literal recursive make keeps exports across -C ──────
        # OpenWrt's metadata scanner invokes literal `make` (not $(MAKE)) from
        # a compound subshell, clears MAKEFLAGS in that subshell, abbreviates
        # the directory flag, and relies on an exported TOPDIR while changing
        # into each package. Its intermediate make selects Bash through
        # `/usr/bin/env bash`; treating that wrapper as a non-POSIX custom
        # interpreter used to discard the generated export environment.
        rcdir = work / "recursive-c-export"
        shutil.rmtree(rcdir, ignore_errors=True)
        (rcdir / "child").mkdir(parents=True, exist_ok=True)
        (rcdir / "rules.mk").write_text("FROM_RULES := reached\n")
        (rcdir / "child/Makefile").write_text(
            "include $(TOPDIR)/rules.mk\n"
            "all:\n\t@printf '%s' '$(FROM_RULES)' > result.txt\n")
        (rcdir / "scan.mk").write_text(
            "SHELL := /usr/bin/env bash\n"
            "all:\n\t@( export MAKEFLAGS= ;make --no-print-dir -r -C child )\n")
        (rcdir / "Makefile").write_text(
            "TOPDIR := $(CURDIR)\n"
            "export TOPDIR\n"
            "all:\n\t@$(MAKE) -r -f scan.mk\n")
        r = run_make("MAKE39", rcdir, "-j10", "all")
        check(r.returncode == 0,
              f"case39: inherited TOPDIR reaches a literal second-level "
              f"recursive make through -C "
              f"(got {r.returncode}: {(r.stdout+r.stderr)[-500:]})")
        sp39 = latest_sqlar(m)
        reached = m.sqlar_content(
            sp39, str((rcdir / "child/result.txt").resolve()).lstrip("/"))
        check(reached == b"reached",
              f"case39: child parsed the parent-rooted include after -C; "
              f"got {reached!r}")
        # ── CASE 40: guarded recursive includes do not lock the AST ───────
        # OpenWrt's package metadata makefiles revisit common include files
        # through nested include layers, with make variables guarding the
        # recursion. GNU make simply evaluates the already-parsed statements
        # again. Holding Kati's parser mutation mutex while evaluating an
        # include instead deadlocks before the inner guard can be checked.
        ri = work / "recursive-include"
        shutil.rmtree(ri, ignore_errors=True)
        ri.mkdir(parents=True, exist_ok=True)
        (ri / "Makefile").write_text(
            "include a.mk\n"
            "all:\n\t@printf '%s' '$(REACHED)' > result.txt\n")
        (ri / "a.mk").write_text(
            "ifndef A_SEEN\n"
            "A_SEEN := 1\n"
            "include b.mk\n"
            "endif\n")
        (ri / "b.mk").write_text(
            "include a.mk\n"
            "REACHED := yes\n")
        r = run_make("MAKE40", ri, "-j10", "all")
        check(r.returncode == 0,
              f"case40: a variable-guarded recursive include completes "
              f"(got {r.returncode}: {(r.stdout+r.stderr)[-500:]})")
        sp40 = latest_sqlar(m)
        reached = m.sqlar_content(
            sp40, str((ri / "result.txt").resolve()).lstrip("/"))
        check(reached == b"yes",
              f"case40: the inner include guard runs and evaluation resumes; "
              f"got {reached!r}")
        # ── CASE 41: recursive argv accepts GNU jobserver transport ────────
        # OpenWrt passes the inherited auth token explicitly to some upstream
        # package makes. It names the same pool already carried in MAKEFLAGS;
        # a recursive make must consume it as control data, not reject it as an
        # unknown goal/flag or append it to $(MAKE) indefinitely.
        js = work / "jobserver-argv"
        shutil.rmtree(js, ignore_errors=True)
        js.mkdir(parents=True, exist_ok=True)
        (js / "child.mk").write_text(
            "all:\n\t@printf joined > result.txt\n")
        (js / "Makefile").write_text(
            "all:\n"
            "\t@$(MAKE) --jobserver-auth=fifo:/.slopbox-jobserver "
            "--jobserver-fds=15,16 -f child.mk\n")
        r = run_make("MAKE41", js, "-j10", "all")
        check(r.returncode == 0,
              f"case41: explicit recursive jobserver authorization is "
              f"accepted (got {r.returncode}: {(r.stdout+r.stderr)[-500:]})")
        sp41 = latest_sqlar(m)
        joined = m.sqlar_content(
            sp41, str((js / "result.txt").resolve()).lstrip("/"))
        check(joined == b"joined",
              f"case41: authorized child make ran on the shared pool; "
              f"got {joined!r}")
        # ── CASE 42: standard compile variables are real relations ─────────
        # Upstream build systems frequently write the same recipe GNU make's
        # implicit rules use, rather than invoking $(CC) themselves.  zstd is
        # one example.  Missing COMPILE.c left -MMD at the start of the command
        # (where it was mistaken for make's ignore-error prefix), while missing
        # COMPILE.S made the shell try to execute the source file.  Exercise an
        # actual compile, not merely variable inspection.
        cv = work / "compile-variables"
        shutil.rmtree(cv, ignore_errors=True)
        cv.mkdir(parents=True, exist_ok=True)
        (cv / "unit.c").write_text("int answer(void) { return 42; }\n")
        (cv / "Makefile").write_text(
            ".PHONY: all\n"
            "DEPFLAGS := -MMD -MP -MF unit.d\n"
            "all: unit.o\n"
            "\t@printf '%s\\n' '$(COMPILE.c)' '$(COMPILE.S)' "
            "'$(OUTPUT_OPTION)' > relations.txt\n"
            "unit.o: unit.c\n"
            "\t@$(COMPILE.c) $(DEPFLAGS) $(OUTPUT_OPTION) $<\n")
        r = run_make("MAKE42", cv, "-j10", "all")
        check(r.returncode == 0,
              f"case42: explicit GNU built-in compile relation executes "
              f"(got {r.returncode}: {(r.stdout+r.stderr)[-500:]})")
        sp42 = latest_sqlar(m)
        obj = m.sqlar_content(
            sp42, str((cv / "unit.o").resolve()).lstrip("/"))
        dep = m.sqlar_content(
            sp42, str((cv / "unit.d").resolve()).lstrip("/"))
        relations = m.sqlar_content(
            sp42, str((cv / "relations.txt").resolve()).lstrip("/"))
        check(bool(obj) and dep is not None and relations is not None
              and b"cc " in relations and b" -c" in relations
              and b"-o all" in relations,
              f"case42: C/C++/assembler recipes share GNU's compile/output "
              f"variables; object={None if obj is None else len(obj)} "
              f"dep={dep!r} relations={relations!r}")
        # ── CASE 43: legacy backquotes preserve escaped backslashes ─────────
        # Libtool's config.status uses this standard double-eval idiom to quote
        # command templates before emitting the executable libtool script.
        # Within legacy backquotes, \\ becomes one backslash. Keeping both made
        # Brush's inner eval fail and stripped every escaped quote from OpenWrt
        # xz's generated libtool, whose later recipes then could not be parsed.
        # This reduced real fragment reparses the generated assignment.
        bq = work / "backquote-libtool"
        shutil.rmtree(bq, ignore_errors=True)
        bq.mkdir(parents=True, exist_ok=True)
        probe = bq / "probe.sh"
        probe.write_text(r'''#!/bin/sh
sed_quote_subst='s/\(["`\$\\]\)/\\\1/g'
double_quote_subst='s/\(["`\\]\)/\\\1/g'
delay_variable_subst='s/\\\\\\\$/\\\$/g'
ECHO='printf %s\n'
SED=sed
archive_expsym_cmds='echo "{ global:" > x~ echo "local: *; };" >> x'
var=archive_expsym_cmds
case `eval \\$ECHO \\""\\$$var"\\"` in
*[\\\`\"\$]*)
  eval "lt_$var=\\\"\`\$ECHO \"\$$var\" | \$SED -e \"\$double_quote_subst\" -e \"\$sed_quote_subst\" -e \"\$delay_variable_subst\"\`\\\""
  ;;
*)
  eval "lt_$var=\\\"\$$var\\\""
  ;;
esac
printf 'archive_expsym_cmds=%s\n' "$lt_archive_expsym_cmds" > generated.sh
sh -n generated.sh
printf preserved > result.txt
'''.replace("`", chr(96)))
        probe.chmod(0o755)
        (bq / "Makefile").write_text(
            "all:\n\t@./probe.sh\n")
        r = run_make("MAKE43", bq, "-j10", "all")
        check(r.returncode == 0,
              f"case43: libtool double-eval fragment preserves syntax "
              f"(got {r.returncode}: {(r.stdout+r.stderr)[-700:]})")
        sp43 = latest_sqlar(m)
        generated = m.sqlar_content(
            sp43, str((bq / "generated.sh").resolve()).lstrip("/"))
        preserved = m.sqlar_content(
            sp43, str((bq / "result.txt").resolve()).lstrip("/"))
        check(generated is not None and b"global:" in generated
              and b"\\\\" in generated
              and preserved == b"preserved",
              f"case43: generated command retains quote escapes and reparses; "
              f"generated={generated!r} result={preserved!r}")
        # ── CASE 44: core implicit link rules produce executables ───────────
        # ELFkickers uses both GNU forms: an explicit target with object
        # prerequisites but no recipe (`objres: objres.o ...`) and one with a
        # source prerequisite (`elfls: elfls.c ...`). COMPILE/LINK variables
        # alone are insufficient unless the standard pattern relations connect
        # those declarations to executable-producing recipes.
        lr = work / "implicit-link"
        shutil.rmtree(lr, ignore_errors=True)
        lr.mkdir(parents=True, exist_ok=True)
        (lr / "object_tool.c").write_text(
            '#include <stdio.h>\nint main(void) { puts("object"); return 0; }\n')
        (lr / "direct_tool.c").write_text(
            '#include <stdio.h>\nint main(void) { puts("direct"); return 0; }\n')
        (lr / "Makefile").write_text(
            ".PHONY: all\n"
            "all: object_tool direct_tool\n"
            "\t@./object_tool > result.txt\n"
            "\t@./direct_tool >> result.txt\n"
            "object_tool: object_tool.o\n"
            "direct_tool: direct_tool.c\n")
        r = run_make("MAKE44", lr, "-j10", "all")
        check(r.returncode == 0,
              f"case44: standard object/source implicit links execute "
              f"(got {r.returncode}: {(r.stdout+r.stderr)[-700:]})")
        object_link_lines = [
            line for line in (r.stdout + r.stderr).splitlines()
            if line.rstrip().endswith("-o object_tool")
        ]
        check(any("object_tool.o" in line and "object_tool.c" not in line
                  for line in object_link_lines),
              f"case44: declared object prerequisite selects the object-link "
              f"relation; lines={object_link_lines!r}")
        sp44 = latest_sqlar(m)
        linked = m.sqlar_content(
            sp44, str((lr / "result.txt").resolve()).lstrip("/"))
        object_tool = m.sqlar_content(
            sp44, str((lr / "object_tool").resolve()).lstrip("/"))
        direct_tool = m.sqlar_content(
            sp44, str((lr / "direct_tool").resolve()).lstrip("/"))
        check(linked == b"object\ndirect\n" and bool(object_tool)
              and bool(direct_tool),
              f"case44: both linked binaries ran; result={linked!r} "
              f"sizes={(len(object_tool) if object_tool else None, len(direct_tool) if direct_tool else None)}")
        # ── CASE 45: GNU --touch refreshes targets without recipes ─────────
        # OpenWrt's Autoconf host build relies on exactly this sequence to
        # suppress maintainer-only manpage regeneration: recursive make first
        # touches stale manuals, then the ordinary compile sees them current.
        tr = work / "touch-mode"
        shutil.rmtree(tr, ignore_errors=True)
        tr.mkdir(parents=True, exist_ok=True)
        (tr / "manual.1").write_text("shipped manual\n")
        (tr / "source.in").write_text("patched source\n")
        os.utime(tr / "manual.1", (1577836800, 1577836800))
        os.utime(tr / "source.in", (1609459200, 1609459200))
        (tr / "Touch.mk").write_text(
            ".PHONY: install-man1 phony\n"
            "install-man1: manual.1 phony\n"
            "manual.1: source.in\n"
            "\t@echo $(error TOUCH-EXPANDED-RECIPE) > recipe-ran\n"
            "phony:\n"
            "\t@echo PHONY-RAN > phony-ran\n"
            "verify: manual.1\n"
            "\t@test ! -e recipe-ran && test ! -e phony-ran\n"
            "\t@printf verified > verified\n")
        (tr / "Makefile").write_text(
            ".PHONY: all\n"
            "all:\n"
            "\t@$(MAKE) --no-print-directory -f Touch.mk --touch install-man1\n"
            "\t@$(MAKE) --no-print-directory -f Touch.mk verify\n")
        r = run_make("MAKE45", tr, "-j10", "all")
        check(r.returncode == 0,
              f"case45: recursive --touch completes without expanding recipes "
              f"(got {r.returncode}: {(r.stdout+r.stderr)[-700:]})")
        check("touch manual.1" in (r.stdout + r.stderr),
              f"case45: touch mode visibly updates the stale manual; "
              f"output={(r.stdout+r.stderr)[-500:]!r}")
        sp45 = latest_sqlar(m)
        manual = m.sqlar_content(
            sp45, str((tr / "manual.1").resolve()).lstrip("/"))
        verified = m.sqlar_content(
            sp45, str((tr / "verified").resolve()).lstrip("/"))
        recipe_ran = m.sqlar_content(
            sp45, str((tr / "recipe-ran").resolve()).lstrip("/"))
        phony_ran = m.sqlar_content(
            sp45, str((tr / "phony-ran").resolve()).lstrip("/"))
        check(manual == b"shipped manual\n" and verified == b"verified"
              and recipe_ran is None and phony_ran is None,
              f"case45: content preserved, later make sees it current, and "
              f"neither ordinary nor phony recipe ran; manual={manual!r} "
              f"verified={verified!r} recipe={recipe_ran!r} phony={phony_ran!r}")
        # ── CASE 46: escaped quotes inside legacy backquotes are syntax ─────
        # CMake's bootstrap emits Ninja dependencies through precisely this
        # construct. POSIX legacy backquotes remove the escape before parsing
        # the nested command, so the quotes group one argument but do not
        # become literal quote bytes in that argument.
        eq = work / "backquote-escaped-quotes"
        shutil.rmtree(eq, ignore_errors=True)
        eq.mkdir(parents=True, exist_ok=True)
        probe = eq / "probe.sh"
        probe.write_text(r'''#!/bin/sh
show_arg() {
  test "$#" = 1 || exit 20
  printf '<%s>' "$1"
}
h='Source/header with space.hxx'
dep="start `show_arg \"${h}\"`"
test "$dep" = 'start <Source/header with space.hxx>' || exit 21
printf '%s\n' "$dep" > result.txt
''')
        probe.chmod(0o755)
        (eq / "Makefile").write_text("all:\n\t@./probe.sh\n")
        r = run_make("MAKE46", eq, "-j10", "all")
        check(r.returncode == 0,
              f"case46: escaped quotes group the nested backquote argument "
              f"without becoming data (got {r.returncode}: "
              f"{(r.stdout+r.stderr)[-700:]})")
        sp46 = latest_sqlar(m)
        quoted = m.sqlar_content(
            sp46, str((eq / "result.txt").resolve()).lstrip("/"))
        check(quoted == b"start <Source/header with space.hxx>\n",
              f"case46: CMake-style nested argument is one unquoted value; "
              f"got {quoted!r}")
        # ── CASE 47: leading ./ goals match canonical rule targets ───────
        # Libtool's bootstrap asks a recursive make to build
        # `./libltdl/Makefile.am`; GNU canonicalizes that spelling to the same
        # identity as the makefile's `libltdl/Makefile.am:` rule.
        cg = work / "canonical-goal"
        shutil.rmtree(cg, ignore_errors=True)
        (cg / "libltdl").mkdir(parents=True)
        (cg / "Makefile").write_text(
            ".PHONY: all\n"
            "all:\n"
            "\t@$(MAKE) --no-print-directory ./libltdl/Makefile.am\n"
            "libltdl/Makefile.am:\n"
            "\t@printf 'goal=<%s> at=<%s>\\n' "
            "'$(MAKECMDGOALS)' '$@' > $@\n")
        r = run_make("MAKE47", cg, "-j10", "all")
        check(r.returncode == 0,
              f"case47: recursive leading-./ goal selects its declared rule "
              f"(got {r.returncode}: {(r.stdout+r.stderr)[-700:]})")
        sp47 = latest_sqlar(m)
        canonical = m.sqlar_content(
            sp47, str((cg / "libltdl/Makefile.am").resolve()).lstrip("/"))
        check(canonical ==
              b"goal=<libltdl/Makefile.am> at=<libltdl/Makefile.am>\n",
              f"case47: command goal and automatic target share GNU's "
              f"canonical identity; got {canonical!r}")
        # ── CASE 48: legacy backquote escape layers around eval ───────────
        # CMake selects source-specific bootstrap flags through a dynamically
        # named variable. The doubled escape keeps the outer parameter literal
        # for eval, while the single escape lets ${a} expand in the command
        # substitution. Losing either layer silently compiles empty objects.
        be = work / "backquote-eval-flags"
        shutil.rmtree(be, ignore_errors=True)
        be.mkdir(parents=True, exist_ok=True)
        probe = be / "probe.sh"
        probe.write_text(r'''#!/bin/sh
cmake_c_flags_String=-DKWSYS_STRING_C
for a in String; do
  src_flags="`eval echo \\${cmake_c_flags_\${a}}` -DKWSYS_NAMESPACE=cmsys"
done
test "$src_flags" = '-DKWSYS_STRING_C -DKWSYS_NAMESPACE=cmsys' || exit 22
printf '%s\n' "$src_flags" > result.txt
''')
        probe.chmod(0o755)
        (be / "Makefile").write_text("all:\n\t@./probe.sh\n")
        r = run_make("MAKE48", be, "-j10", "all")
        check(r.returncode == 0,
              f"case48: nested legacy escapes feed eval a dynamic variable "
              f"name (got {r.returncode}: {(r.stdout+r.stderr)[-700:]})")
        sp48 = latest_sqlar(m)
        flags = m.sqlar_content(
            sp48, str((be / "result.txt").resolve()).lstrip("/"))
        check(flags == b"-DKWSYS_STRING_C -DKWSYS_NAMESPACE=cmsys\n",
              f"case48: CMake source flags survive both expansion layers; "
              f"got {flags!r}")
        # ── CASE 49: an unchanged remade makefile has converged ──────────
        # Autotools attaches generated makefile fragments to rebuild recipes
        # which can run, inspect an embedded revision, and intentionally leave
        # the target untouched. GNU restarts only if the included makefile's
        # timestamp changed; command execution somewhere in its dependency
        # graph is not itself a reason to reparse forever.
        ur = work / "unchanged-remade-include"
        shutil.rmtree(ur, ignore_errors=True)
        ur.mkdir(parents=True, exist_ok=True)
        (ur / "generated.mk").write_text("VALUE := stable\n")
        (ur / "GNUmakefile").write_text(
            "include generated.mk\n"
            ".PHONY: force all\n"
            "generated.mk: force\n"
            "\t@test '$(VALUE)' = stable\n"
            "all:\n"
            "\t@printf '%s' '$(VALUE)' > result.txt\n")
        r = run_make("MAKE49", ur, "-j10", "all")
        check(r.returncode == 0,
              f"case49: unchanged included makefile does not exhaust the "
              f"remake loop (got {r.returncode}: "
              f"{(r.stdout+r.stderr)[-700:]})")
        sp49 = latest_sqlar(m)
        stable = m.sqlar_content(
            sp49, str((ur / "result.txt").resolve()).lstrip("/"))
        check(stable == b"stable",
              f"case49: evaluation continues with the existing include; "
              f"got {stable!r}")
        # ── CASE 50: prerequisite-only rules inherit suffix recipes ───────
        # Elfutils' generated parser objects have extra explicit prerequisites
        # but no explicit commands. GNU combines those prerequisites with the
        # `.c.o` recipe, and may first make the missing C source through
        # `.l.c`/`.y.c`. Both the declared and wholly implied intermediate
        # forms must therefore traverse the ordinary suffix-rule chain.
        sc = work / "suffix-chain-explicit-prereqs"
        shutil.rmtree(sc, ignore_errors=True)
        sc.mkdir(parents=True, exist_ok=True)
        (sc / "declared.src").write_text("declared\n")
        (sc / "implied.src").write_text("implied\n")
        (sc / "paired.src").write_text("paired\n")
        (sc / "marker").write_text("marker\n")
        (sc / "Makefile").write_text(
            ".SUFFIXES:\n"
            ".SUFFIXES: .src .mid .o\n"
            ".src.mid:\n"
            "\t@cp $< $@\n"
            ".mid.o:\n"
            "\t@cp $< $@\n"
            "%.pic: %.src %.o\n"
            "\t@cp $< $@\n"
            ".PHONY: all\n"
            "all: declared.o implied.o paired.pic\n"
            "\t@cat declared.o implied.o paired.pic > result.txt\n"
            "declared.o: declared.mid marker\n"
            "implied.o: marker\n")
        r = run_make("MAKE50", sc, "-j10", "all")
        check(r.returncode == 0,
              f"case50: prerequisite-only targets compose with chained "
              f"suffix recipes (got {r.returncode}: "
              f"{(r.stdout+r.stderr)[-700:]})")
        sp50 = latest_sqlar(m)
        chained = m.sqlar_content(
            sp50, str((sc / "result.txt").resolve()).lstrip("/"))
        declared_mid = m.sqlar_content(
            sp50, str((sc / "declared.mid").resolve()).lstrip("/"))
        implied_mid = m.sqlar_content(
            sp50, str((sc / "implied.mid").resolve()).lstrip("/"))
        paired_object = m.sqlar_content(
            sp50, str((sc / "paired.o").resolve()).lstrip("/"))
        check(chained == b"declared\nimplied\npaired\n"
              and declared_mid == b"declared\n"
              and implied_mid == b"implied\n"
              and paired_object == b"paired\n",
              f"case50: declared/implied intermediates and a pattern's "
              f"suffix-produced prerequisite were built; result={chained!r} "
              f"mids={(declared_mid, implied_mid)!r} object={paired_object!r}")
        # ── CASE 51: repeated command-line += survives recursion ──────────
        # OpenWrt's elfutils host build passes a sequence of LIBS+= expressions
        # which must all remain deferred until each Automake subdirectory sets
        # `subdir`. Treating definitions as replace-by-name retained only the
        # last wildcard and omitted libgnu.a from final links.
        ca = work / "recursive-command-append"
        shutil.rmtree(ca, ignore_errors=True)
        (ca / "child").mkdir(parents=True)
        (ca / "Makefile").write_text(
            ".PHONY: all\n"
            "all:\n"
            "\t@$(MAKE) --no-print-directory -C child\n")
        (ca / "child" / "xsize.o").write_text("object\n")
        (ca / "child" / "Makefile").write_text(
            "subdir := src\n"
            ".PHONY: all\n"
            "all:\n"
            "\t@printf '%s\\n' '$(LIBS)' > result.txt\n")
        r = run_make(
            "MAKE51", ca, "-j10",
            "LIBS+=base",
            "LIBS+=$(if $(findstring src,$(subdir)),conditional)",
            "LIBS+=$(wildcard xsize.o)",
            "all")
        check(r.returncode == 0,
              f"case51: recursive command-line appends execute "
              f"(got {r.returncode}: {(r.stdout+r.stderr)[-700:]})")
        sp51 = latest_sqlar(m)
        recursive_libs = m.sqlar_content(
            sp51, str((ca / "child/result.txt").resolve()).lstrip("/"))
        check(recursive_libs == b"base conditional xsize.o\n",
              f"case51: every += is preserved and expanded in child scope; "
              f"got {recursive_libs!r}")
        # ── CASE 52: unquoted legacy backquotes retain escaped quotes ──────
        # Quilt's configure asks a selected Bash to run a script containing a
        # nested command substitution. In an unquoted legacy backquote, \"
        # remains escaped command text; only a backquote inside double quotes
        # consumes that escape. Treating both contexts alike made the inner
        # `bash -c` text syntactically incomplete.
        uq = work / "backquote-unquoted-escaped-quotes"
        shutil.rmtree(uq, ignore_errors=True)
        uq.mkdir(parents=True, exist_ok=True)
        probe = uq / "probe.sh"
        probe.write_text(r'''#!/bin/sh
BASH=/bin/bash
if test `$BASH -c "echo \"\\\$(set -- \\\$'a b'; echo \\\$#)\"" 2>/dev/null` = "1"; then
  printf preserved > result.txt
else
  exit 23
fi
''')
        probe.chmod(0o755)
        (uq / "Makefile").write_text("all:\n\t@./probe.sh\n")
        r = run_make("MAKE52", uq, "-j10", "all")
        check(r.returncode == 0,
              f"case52: Quilt-style unquoted backquote executes "
              f"(got {r.returncode}: {(r.stdout+r.stderr)[-700:]})")
        sp52 = latest_sqlar(m)
        quote_result = m.sqlar_content(
            sp52, str((uq / "result.txt").resolve()).lstrip("/"))
        check(quote_result == b"preserved",
              f"case52: nested Bash saw one positional parameter; "
              f"got {quote_result!r}")
        # ── CASE 53: function arguments preserve escaped recipe hashes ────
        # U-Boot's filechk macro emits C preprocessor lines. GNU retains the
        # backslash on `#` inside an $(if ...) argument until the shell parses
        # the recipe; stripping it while parsing the function turns the rest
        # of the one-line recipe into a shell comment and loses the closing
        # subshell parenthesis.
        fh = work / "function-escaped-hash"
        shutil.rmtree(fh, ignore_errors=True)
        fh.mkdir(parents=True, exist_ok=True)
        (fh / "Makefile").write_text(
            "define FRAGMENT\n"
            "(echo \\#define DIRECT; "
            "$(if yes,echo \\#include \\<configs/test.h\\>;))\n"
            "endef\n"
            "all:\n"
            "\t@$(FRAGMENT) > result.txt\n")
        r = run_make("MAKE53", fh, "-j10", "all")
        check(r.returncode == 0,
              f"case53: escaped hash survives nested make function "
              f"(got {r.returncode}: {(r.stdout+r.stderr)[-700:]})")
        sp53 = latest_sqlar(m)
        header = m.sqlar_content(
            sp53, str((fh / "result.txt").resolve()).lstrip("/"))
        check(header == b"#define DIRECT\n#include <configs/test.h>\n",
              f"case53: both direct and function-produced header lines "
              f"reach the shell; got {header!r}")
        # ── CASE 54: reject incomplete implicit-rule chains ───────────────
        # Binutils and GDB ship generated Texinfo sources alongside rules that
        # can regenerate most other manuals from a same-stem C or header file.
        # A matching intermediate pattern is not sufficient: GNU verifies the
        # entire chain before selecting it. The shipped bfdsumm.texi must stay
        # an ordinary existing file when neither bfdsumm.c nor bfdsumm.h exists.
        iv = work / "implicit-chain-viability"
        shutil.rmtree(iv, ignore_errors=True)
        (iv / "doc").mkdir(parents=True)
        (iv / "doc/manual.texi").write_text("shipped manual\n")
        (iv / "Makefile").write_text(
            ".PHONY: all\n"
            "all: doc/manual.texi\n"
            "\t@cat $< > result.txt\n"
            ".PRECIOUS: doc/%.stamp\n"
            "doc/%.texi: doc/%.stamp ; @true\n"
            "doc/%.stamp: %.c\n"
            "\t@cp $< $@\n"
            "doc/%.stamp: %.h\n"
            "\t@cp $< $@\n")
        r = run_make("MAKE54", iv, "-j10", "all")
        check(r.returncode == 0,
              f"case54: incomplete implicit chain is rejected before it "
              f"enters the graph (got {r.returncode}: "
              f"{(r.stdout+r.stderr)[-700:]})")
        sp54 = latest_sqlar(m)
        shipped_manual = m.sqlar_content(
            sp54, str((iv / "result.txt").resolve()).lstrip("/"))
        generated_stamp = m.sqlar_content(
            sp54, str((iv / "doc/manual.stamp").resolve()).lstrip("/"))
        check(shipped_manual == b"shipped manual\n" and generated_stamp is None,
              f"case54: existing generated source is consumed unchanged; "
              f"result={shipped_manual!r} stamp={generated_stamp!r}")
        # ── CASE 55: recipe-less pattern rules are cancellations ─────────
        # Automake emits `%.o: %.m` without a recipe to cancel make's
        # built-in Objective-C relation. It is not a successful empty recipe:
        # when both same-stem .m and .c files exist, the declared .c.o suffix
        # rule must still compile the object. Cancellation also removes an
        # earlier identical user pattern instead of leaving either candidate.
        pc = work / "pattern-rule-cancellation"
        shutil.rmtree(pc, ignore_errors=True)
        pc.mkdir(parents=True)
        (pc / "suffix.c").write_text("from-c\n")
        (pc / "suffix.m").write_text("from-m\n")
        (pc / "pattern.good").write_text("from-good\n")
        (pc / "pattern.bad").write_text("from-bad\n")
        (pc / "Makefile").write_text(
            ".SUFFIXES:\n"
            ".SUFFIXES: .c .m .o\n"
            ".c.o:\n"
            "\t@cp $< $@\n"
            "%.o: %.m\n"
            "%.out: %.good\n"
            "\t@cp $< $@\n"
            "%.out: %.bad\n"
            "\t@cp $< $@\n"
            "%.out: %.bad\n"
            ".PHONY: all\n"
            "all: suffix.o pattern.out\n"
            "\t@cat $^ > result.txt\n")
        r = run_make("MAKE55", pc, "-j10", "all")
        check(r.returncode == 0,
              f"case55: cancelled implicit patterns fall through to valid "
              f"relations (got {r.returncode}: {(r.stdout+r.stderr)[-700:]})")
        sp55 = latest_sqlar(m)
        cancellation_result = m.sqlar_content(
            sp55, str((pc / "result.txt").resolve()).lstrip("/"))
        check(cancellation_result == b"from-c\nfrom-good\n",
              f"case55: suffix and surviving pattern recipes produced the "
              f"outputs; got {cancellation_result!r}")
        # ── CASE 56: recursive make completes every sibling prerequisite ──
        # OpenWrt wraps recursive makes in an external timing program.  The
        # child `compile` aggregate has both a build stamp and an install stamp;
        # its parent may proceed only after BOTH recipes have completed.  This
        # exercises the process-shadow door (the external wrapper resolves
        # `make` through Sarun's FUSE shadow), not merely a directly invoked
        # in-process MakeBuiltin.
        ri = work / "recursive-install-boundary"
        shutil.rmtree(ri, ignore_errors=True)
        (ri / "child" / "grand").mkdir(parents=True)
        (ri / "Makefile").write_text(
            ".PHONY: all bootstrap\n"
            "all: bootstrap\n"
            "\t@test -f child/installed; cat child/installed > result.txt\n"
            "bootstrap:\n"
            "\t@python3 -c 'import subprocess,sys; "
            "sys.exit(subprocess.call(sys.argv[1:]))' "
            "make --no-print-directory -C child -j10 compile\n")
        (ri / "child" / "Makefile").write_text(
            ".NOTPARALLEL:\n"
            "define BuildRules\n"
            ".PHONY: compile aggregate\n"
            "compile: aggregate\n"
            "aggregate: built installed\n"
            "built:\n"
            "\t@+$$(MAKE) --no-print-directory -C grand all\n"
            "\t@sleep 0.2; printf built > $$@\n"
            "installed: built\n"
            "\t@sleep 0.2; printf installed > $$@\n"
            "endef\n"
            "$(eval $(call BuildRules))\n")
        (ri / "child" / "grand" / "Makefile").write_text(
            ".PHONY: all\n"
            "all:\n"
            "\t@exec true\n")
        r = run_make("MAKE56", ri, "-j10", "all")
        check(r.returncode == 0,
              f"case56: wrapped recursive make waits through install "
              f"boundary (got {r.returncode}: {(r.stdout+r.stderr)[-700:]})")
        sp56 = latest_sqlar(m)
        install_result = m.sqlar_content(
            sp56, str((ri / "result.txt").resolve()).lstrip("/"))
        check(install_result == b"installed",
              f"case56: parent observes child install result; "
              f"got {install_result!r}")
        # ── CASE 57: pipeline stdin crosses an embedded make boundary ──
        # OpenWrt configures kernel headers with `yes '' | make oldconfig`.
        # The make builtin is a Brush pipeline stage, and recipes launched by
        # its parallel scheduler must inherit that logical pipe reader rather
        # than the engine process's terminal stdin.
        pi = work / "recursive-pipeline-stdin"
        shutil.rmtree(pi, ignore_errors=True)
        (pi / "child").mkdir(parents=True)
        (pi / "Makefile").write_text(
            ".PHONY: all\n"
            "all:\n"
            "\t@printf 'from-pipe\\n' | $(MAKE) --no-print-directory "
            "-C child result\n"
            "\t@cat child/result > observed\n")
        (pi / "child" / "Makefile").write_text(
            ".PHONY: result\n"
            "result:\n"
            "\t@IFS= read -r value; printf '%s\\n' \"$$value\" > result\n")
        r = run_make("MAKE57", pi, "-j10", "all")
        check(r.returncode == 0,
              f"case57: piped recursive make completes (got {r.returncode}: "
              f"{(r.stdout+r.stderr)[-700:]})")
        sp57 = latest_sqlar(m)
        piped_result = m.sqlar_content(
            sp57, str((pi / "observed").resolve()).lstrip("/"))
        check(piped_result == b"from-pipe\n",
              f"case57: child recipe inherits make's logical stdin; "
              f"got {piped_result!r}")
        # ── CASE 58: MAKEOVERRIDES stops command variables at a boundary ─
        # GCC's top-level recursive build passes a temporary CXX override into
        # an intermediate make, which clears MAKEOVERRIDES before descending
        # into a configured subdirectory.  The leaf must see its file-origin
        # compiler, not the now-empty parent command-line override.
        mo = work / "recursive-makeoverrides-boundary"
        shutil.rmtree(mo, ignore_errors=True)
        (mo / "middle" / "leaf").mkdir(parents=True)
        (mo / "Makefile").write_text(
            ".PHONY: all\n"
            "all:\n"
            "\t@$(MAKE) --no-print-directory -C middle "
            "'CXX=$$(MISSING_CXX)' all\n")
        (mo / "middle" / "Makefile").write_text(
            "MAKEOVERRIDES =\n"
            ".PHONY: all\n"
            "all:\n"
            "\t@$(MAKE) --no-print-directory -C leaf all\n")
        (mo / "middle" / "leaf" / "Makefile").write_text(
            "CXX = configured-cxx\n"
            ".PHONY: all\n"
            "all:\n"
            "\t@printf '%s|%s\\n' '$(CXX)' '$(origin CXX)' > result.txt\n")
        r = run_make("MAKE58", mo, "-j10", "all")
        check(r.returncode == 0,
              f"case58: recursive MAKEOVERRIDES boundary completes "
              f"(got {r.returncode}: {(r.stdout+r.stderr)[-700:]})")
        sp58 = latest_sqlar(m)
        overrides_result = m.sqlar_content(
            sp58,
            str((mo / "middle/leaf/result.txt").resolve()).lstrip("/"))
        check(overrides_result == b"configured-cxx|file\n",
              f"case58: leaf uses its configured file-origin compiler; "
              f"got {overrides_result!r}")
        # ── CASE 59: GNU's default RM variable is available ─────────
        # OpenWrt uses $(RM) in final toolchain install recipes without defining
        # it itself. GNU make's built-in value is `rm -f`; an empty value turns
        # the following pathname into a command and fails after a long build.
        rmv = work / "default-rm-variable"
        shutil.rmtree(rmv, ignore_errors=True)
        rmv.mkdir(parents=True)
        (rmv / "Makefile").write_text(
            ".PHONY: all\n"
            "all:\n"
            "\t@printf old > victim\n"
            "\t@$(RM) victim\n"
            "\t@test ! -e victim; printf removed > result.txt\n")
        r = run_make("MAKE59", rmv, "-j10", "all")
        check(r.returncode == 0,
              f"case59: default RM recipe completes (got {r.returncode}: "
              f"{(r.stdout+r.stderr)[-700:]})")
        sp59 = latest_sqlar(m)
        rm_result = m.sqlar_content(
            sp59, str((rmv / "result.txt").resolve()).lstrip("/"))
        victim = m.sqlar_content(
            sp59, str((rmv / "victim").resolve()).lstrip("/"))
        check(rm_result == b"removed" and victim is None,
              f"case59: $(RM) removes the requested file; "
              f"result={rm_result!r} victim={victim!r}")
        # ── CASE 60: tabs delimit make function names ───────────────
        # Linux's arm64 stack-protector preparation uses a tab between
        # `shell` and its multiline argument. GNU accepts all whitespace here;
        # parsing only a literal space leaves the outer ')' as a shell recipe.
        tabfn = work / "tab-separated-make-function"
        shutil.rmtree(tabfn, ignore_errors=True)
        tabfn.mkdir(parents=True)
        (tabfn / "Makefile").write_text(
            ".PHONY: all prepare stack_protector_prepare prepare0\n"
            "ifeq ($(CONFIG_STACKPROTECTOR_PER_TASK),y)\n"
            "prepare: stack_protector_prepare\n"
            "stack_protector_prepare: prepare0 offsets.h\n"
            "\t$(eval KBUILD_CFLAGS += -mstack-protector-guard=sysreg\t\t  \\\n"
            "\t\t\t\t-mstack-protector-guard-reg=sp_el0\t  \\\n"
            "\t\t\t\t-mstack-protector-guard-offset=$(shell\t  \\\n"
            "\t\t\tawk '{if ($$2 == \"TSK_STACK_CANARY\") print $$3;}' \\\n"
            "\t\t\t\t\toffsets.h))\n"
            "endif\n"
            "prepare0:\n"
            "offsets.h:\n"
            "\t@printf 'x TSK_STACK_CANARY 1224\\n' > $@\n"
            "all: prepare\n"
            "\t@printf '%s\\n' '$(KBUILD_CFLAGS)' > result.txt\n")
        r = run_make("MAKE60", tabfn, "-j10",
                     "CONFIG_STACKPROTECTOR_PER_TASK=y", "all")
        check(r.returncode == 0,
              f"case60: tab-delimited multiline make function completes "
              f"(got {r.returncode}: {(r.stdout+r.stderr)[-700:]})")
        sp60 = latest_sqlar(m)
        tab_result = m.sqlar_content(
            sp60, str((tabfn / "result.txt").resolve()).lstrip("/"))
        check(tab_result ==
              b"-mstack-protector-guard=sysreg "
              b"-mstack-protector-guard-reg=sp_el0 "
              b"-mstack-protector-guard-offset=1224\n",
              f"case60: eval consumes the recipe and appends the shell result; "
              f"got {tab_result!r}")
        # ── CASE 61: recipe eval refreshes recursive-make exports ───
        # Linux exports KBUILD_CFLAGS, then its stack_protector_prepare recipe
        # appends the per-task canary options with $(eval). A later recursive
        # Kbuild must inherit that new value, not the prefix snapshotted after
        # parsing; otherwise modules reference the global __stack_chk_guard.
        evalexport = work / "recipe-eval-export"
        shutil.rmtree(evalexport, ignore_errors=True)
        (evalexport / "sub").mkdir(parents=True)
        (evalexport / "Makefile").write_text(
            "export KBUILD_CFLAGS\n"
            ".PHONY: all prepare stack_protector_prepare prepare0\n"
            "prepare: stack_protector_prepare\n"
            "stack_protector_prepare: prepare0\n"
            "\t$(eval KBUILD_CFLAGS += -mstack-protector-guard=sysreg)\n"
            "prepare0:\n"
            "all: prepare\n"
            "\t@$(MAKE) --no-print-directory -C sub all\n")
        (evalexport / "sub" / "Makefile").write_text(
            ".PHONY: all\n"
            "all:\n"
            "\t@printf '%s|%s\\n' '$(KBUILD_CFLAGS)' "
            "'$(origin KBUILD_CFLAGS)' > result.txt\n")
        r = run_make("MAKE61", evalexport, "-j10", "all")
        check(r.returncode == 0,
              f"case61: recursive make after recipe eval completes "
              f"(got {r.returncode}: {(r.stdout+r.stderr)[-700:]})")
        sp61 = latest_sqlar(m)
        evalexport_result = m.sqlar_content(
            sp61, str((evalexport / "sub/result.txt").resolve()).lstrip("/"))
        check(evalexport_result ==
              b"-mstack-protector-guard=sysreg|environment\n",
              f"case61: recipe eval refreshes the exported child value; "
              f"got {evalexport_result!r}")
    finally:
        if eng is not None and eng.poll() is None:
            eng.terminate()
            try: eng.wait(timeout=10)
            except Exception: eng.kill()
        os.environ.pop("SLOPBOX_NS", None)
        shutil.rmtree(tmp, ignore_errors=True)
        shutil.rmtree(work, ignore_errors=True)
    print("\n" + ("MAKE-RS PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_make_rs():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
