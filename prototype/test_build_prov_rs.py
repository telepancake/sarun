#!/usr/bin/env python3
"""Build-provenance recording is a FEATURE, not a side effect — this file
pins it for BOTH backends so it cannot silently regress again.

For each backend (FUSE overlay `-b`, and sud `--sud -b`) it runs:

  make:   a 3-edge Makefile with -j4 — asserts the box sqlar holds
          build_edges rows for every edge AND brushprov pipeline rows
          (the l and g screens' data).
  ninja:  a 2-edge build.ninja with -j4 — same assertions through n2.
  flags:  the full user-reported combination `--sud -b -N --vars` —
          the recording must not depend on net mode or --vars.
  wedge:  `{ find . ; } | while read | wc -l` must COMPLETE (the
          compound-pipeline-stage starvation wedge, brush-core patch
          0150) — guarded by an in-box timeout so a regression fails
          fast instead of hanging the suite.

History: sud recorded NO pipelines and NO build edges (no FD broker in
a sud box; fixed by SARUN_SUD_PROV direct dial) and nothing caught it —
test_make_rs/test_n2_rs assert edges only through FUSE boxes. This file
is the missing cross-backend net.

Deps: uv run --with "wcmatch>=8.4" --with "python-magic>=0.4" \
        python test_build_prov_rs.py
Needs `make engine` (the sud cases skip if sud64 is missing), FUSE +
bwrap for the -b box.
"""
import os, shutil, socket, sqlite3, subprocess, sys, tempfile, time
from pathlib import Path
from importlib.machinery import SourceFileLoader

_HERE = Path(__file__).resolve().parent
SARUN = str(_HERE / "libtestsarun.py")
BIN = _HERE.parent / "engine/target/x86_64-unknown-linux-musl/release/sarun"
SUD64 = BIN.parent / "sud64"

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


def wait_socket(sock, timeout=30):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.settimeout(1.0); s.connect(sock); return True
        except OSError:
            time.sleep(0.1)
    return False


def latest_sqlar():
    return max(Path(os.environ["XDG_STATE_HOME"]).joinpath("slopbox.BPROV")
               .glob("*.sqlar"), key=lambda p: int(p.stem))


def table_count(sp, table):
    con = sqlite3.connect(f"file:{sp}?mode=ro", uri=True)
    try:
        names = [r[0] for r in con.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name=?",
            (table,))]
        if not names:
            return None
        return con.execute(f"SELECT count(*) FROM {table}").fetchone()[0]
    finally:
        con.close()


def run_box(backend, name, work, cmd, extra_flags=(), timeout=300):
    flags = {"fuse": ["-b", "--net", "off"],
             "sud":  ["--sud", "-b", "--net", "off"]}[backend]
    return subprocess.run(
        [str(BIN), "run", *flags, *extra_flags, name, "-C", str(work),
         "--", *cmd],
        capture_output=True, text=True, timeout=timeout)


def main():
    if not BIN.exists():
        raise SystemExit("test_build_prov_rs: engine binary unavailable — "
                         "run `make engine`")
    backends = ["fuse"]
    if SUD64.exists():
        backends.append("sud")
    else:
        print("  ok  (sud cases SKIPPED: sud64 missing — `make engine`)")

    tmp = Path(tempfile.mkdtemp(prefix="bprov-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "BPROV"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()

    # Box-visible work trees under /root (host /tmp is hidden box-side).
    work = Path("/root/bprov_work")
    shutil.rmtree(work, ignore_errors=True)
    (work / "sub").mkdir(parents=True)
    (work / "a.src").write_text("A\n")
    (work / "b.src").write_text("B\n")
    (work / "Makefile").write_text(
        "all: out\n"
        "out: a.mid b.mid\n\tcat a.mid b.mid > out\n"
        "%.mid: %.src\n\tcp $< $@\n")
    (work / "build.ninja").write_text(
        "rule cp\n  command = cp $in $out\n"
        "rule cat\n  command = cat $in > $out\n"
        "build a.o: cp a.src\nbuild nout: cat a.o\n")
    (work / "sub" / "x").write_text("x\n")

    eng = None
    try:
        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE,
                               stderr=subprocess.STDOUT)
        if not wait_socket(m.sock_path()):
            raise RuntimeError("engine socket never appeared")

        for backend in backends:
            # ── make -j4: edges + pipelines recorded ─────────────────────
            for f in ("a.mid", "b.mid", "out"):
                (work / f).unlink(missing_ok=True)
            r = run_box(backend, f"BP-MAKE-{backend}", work,
                        ["make", "-j4"])
            check(r.returncode == 0,
                  f"{backend}: make -j4 exits 0 "
                  f"(got {r.returncode}: {r.stderr[-400:]})")
            sp = latest_sqlar()
            edges = table_count(sp, "build_edges")
            prov = table_count(sp, "brushprov")
            check(edges is not None and edges >= 4,
                  f"{backend}: build_edges recorded for make "
                  f"(all/out/a.mid/b.mid; got {edges})")
            check(prov is not None and prov >= 3,
                  f"{backend}: brushprov pipelines recorded for make "
                  f"(got {prov})")

            # ── ninja -j4 (n2): same recording ───────────────────────────
            for f in ("a.o", "nout"):
                (work / f).unlink(missing_ok=True)
            r = run_box(backend, f"BP-NINJA-{backend}", work,
                        ["ninja", "-j4"])
            check(r.returncode == 0,
                  f"{backend}: ninja -j4 exits 0 "
                  f"(got {r.returncode}: {r.stderr[-400:]})")
            sp = latest_sqlar()
            edges = table_count(sp, "build_edges")
            prov = table_count(sp, "brushprov")
            check(edges is not None and edges >= 2,
                  f"{backend}: build_edges recorded for ninja (got {edges})")
            check(prov is not None and prov >= 2,
                  f"{backend}: brushprov pipelines recorded for ninja "
                  f"(got {prov})")

            # ── compound-stage pipeline completes (wedge regression) ─────
            r = run_box(backend, f"BP-WEDGE-{backend}", work,
                        ["sh", "-c",
                         "/usr/bin/timeout 60 sh -c "
                         "'{ find . ; } | while read -r d; do echo x; done "
                         "| wc -l'; echo wedge-rc=$?"])
            check("wedge-rc=0" in r.stdout,
                  f"{backend}: compound pipeline stage completes "
                  f"(no starvation wedge; out={r.stdout[-200:]!r})")

        # ── shadow_make.glob is honored by BOTH backends ─────────────────
        # A make at a NONSTANDARD path, matched only by a user glob line:
        # it must be the embedded kati (edges recorded, no real `make`
        # process) under sud exactly as under FUSE. This was the second
        # way edges silently vanished: sud hardcoded /bin,/usr/bin and
        # ignored the config, so a globbed make ran REAL — processes
        # recorded, zero edges, zero recipe pipelines.
        mytools = Path("/root/bprov_mytools/bin")
        mytools.mkdir(parents=True, exist_ok=True)
        shutil.copy2("/usr/bin/make", mytools / "make")
        globf = Path(os.environ["XDG_CONFIG_HOME"]) / "slopbox.BPROV" \
            / "shadow_make.glob"
        globf.parent.mkdir(parents=True, exist_ok=True)
        globf.write_text("/bin/make\n/usr/bin/make\n"
                         "/root/bprov_mytools/**/make\n")
        try:
            for backend in backends:
                for f in ("a.mid", "b.mid", "out"):
                    (work / f).unlink(missing_ok=True)
                r = run_box(backend, f"BP-GLOB-{backend}", work,
                            ["sh", "-c",
                             "PATH=/root/bprov_mytools/bin:$PATH make -j4"])
                check(r.returncode == 0,
                      f"{backend}: globbed-path make exits 0 "
                      f"(got {r.returncode}: {r.stderr[-400:]})")
                sp = latest_sqlar()
                edges = table_count(sp, "build_edges")
                check(edges is not None and edges >= 4,
                      f"{backend}: shadow_make.glob-matched make records "
                      f"build_edges (got {edges})")
        finally:
            globf.unlink(missing_ok=True)
            shutil.rmtree(mytools.parent, ignore_errors=True)

        # ── the exact user-reported flag combo on sud ────────────────────
        if "sud" in backends:
            for f in ("a.mid", "b.mid", "out"):
                (work / f).unlink(missing_ok=True)
            r = run_box("sud", "BP-FLAGS", work, ["make", "-j4"],
                        extra_flags=["-N", "--vars"])
            # -N shares the host netns; strip the --net flag conflict.
            if r.returncode != 0 and "--net" in (r.stderr or ""):
                r = subprocess.run(
                    [str(BIN), "run", "--sud", "-b", "-N", "--vars",
                     "BP-FLAGS2", "-C", str(work), "--", "make", "-j4"],
                    capture_output=True, text=True, timeout=300)
            check(r.returncode == 0,
                  f"sud -N --vars: make -j4 exits 0 "
                  f"(got {r.returncode}: {r.stderr[-400:]})")
            sp = latest_sqlar()
            edges = table_count(sp, "build_edges")
            check(edges is not None and edges >= 4,
                  f"sud -N --vars: build_edges recorded (got {edges})")
    finally:
        if eng is not None:
            eng.terminate()
            try:
                eng.wait(timeout=10)
            except subprocess.TimeoutExpired:
                eng.kill()
        shutil.rmtree(work, ignore_errors=True)

    if _fails:
        print(f"\nBUILD-PROV FAIL ({len(_fails)}):")
        for f in _fails:
            print("  - " + f)
        raise SystemExit(1)
    print("\nBUILD-PROV PASS")


def test_build_prov_rs():
    main()


if __name__ == "__main__":
    main()
