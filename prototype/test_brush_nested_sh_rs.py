#!/usr/bin/env python3
"""D9 follow-on — NESTED-shell IS-brush for the RUST engine (engine/).

The previous round did OBSERVE-ONLY interposition: it parsed a nested `sh -c`
just for provenance, then exec'd the REAL /bin/sh. That can never see what a
real shell actually did with builtins / vars / control flow / expansions.

This round flips it: a -b brush box's /bin/sh, /bin/bash (and the /usr/bin/
aliases) are shadowed by the engine binary, and the brush-sh shim RUNS the
nested recipe THROUGH embedded brush-core. There is NO real-shell fallback —
anything brush cannot run is a VISIBLE error and a non-zero exit, matching the
D9 explicit-toggle rule that already governs the top-level brush body.

Cases verified (all real, against the released engine binary):
  1. Nested `sh -c 'echo nested > /root/n.txt'`: file is written, a brushprov
     row exists for the recipe with nested=1, parsed pipeline structure
     matches the source.
  2. Builtins really run THROUGH brush (not silently dropped): a nested
     `sh -c 'X=1; cd /tmp; pwd; export Y=2; echo $X-$Y > /root/vars.txt'`
     writes "1-2" and produces a brushprov row — proves cd / X=/export / $X
     expansion are executed by brush in this process, not by /bin/sh.
  3. Visible failure / no fallback: a nested `sh -c` containing process
     substitution `<(…)` — which brush-core sh-mode does NOT parse — exits
     non-zero with a stderr message; NO /bin/sh fallback runs the recipe.
  4. Negative: a non-brush box has no shadow binds; its /bin/sh is the real
     system shell (no brushprov rows on the box).
  5. Sanity: a top-level -b box still runs end-to-end (writes captured,
     top-level brushprov rows present).

Run:
    uv run --with "pyfuse3>=3.2" --with "trio>=0.22" --with "wcmatch>=8.4" \
      --with "python-magic>=0.4" python test_brush_nested_sh_rs.py
Skips (passes vacuously) if cargo/the binary are unavailable.
"""
import os, shutil, socket, sqlite3, subprocess, sys, tempfile, time
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


def main():
    if not ensure_binary():
        raise SystemExit("test_brush_nested_sh_rs: engine binary unavailable — run `make engine`")
    tmp = Path(tempfile.mkdtemp(prefix="brushnestrs-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "RS"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    eng = None
    # Host paths the box must NEVER touch (every write should land in overlay).
    host_paths = [Path(p) for p in ("/root/top.txt", "/root/nested.txt",
                                    "/root/vars.txt", "/root/bad.txt",
                                    "/root/neg.txt", "/root/f.txt",
                                    "/root/bash.txt", "/root/shbad.txt")]
    for h in host_paths: h.unlink(missing_ok=True)
    try:
        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        sock = m.sock_path()
        if not wait_socket(sock):
            raise RuntimeError("rust engine socket never appeared")

        # ── CASE 1+5: top-level brush body + nested `sh -c` write ───────────
        r = subprocess.run(
            [str(BIN), "run", "-b", "NEST", "--",
             "sh", "-c",
             "echo top > /root/top.txt; "
             "/bin/sh -c 'echo nested > /root/nested.txt'"],
            capture_output=True, text=True, timeout=120)
        check(r.returncode == 0,
              f"case1: -b box w/ nested sh -c exits 0 (got {r.returncode}: "
              f"{r.stderr[-400:]})")
        check(not any(h.exists() for h in host_paths),
              "case1: nothing leaks to the host fs")

        sp = latest_sqlar(m)
        check(m.sqlar_content(sp, "root/top.txt") == b"top\n",
              "case5: top-level write captured ('top')")
        check(m.sqlar_content(sp, "root/nested.txt") == b"nested\n",
              "case1: nested-recipe write captured ('nested') — brush ran it")

        con = sqlite3.connect(f"file:{sp}?mode=ro", uri=True)
        try:
            rows = con.execute(
                "SELECT cmd, nested, record FROM brushprov ORDER BY id").fetchall()
        finally:
            con.close()
        top_rows = [(c, rj) for (c, n, rj) in rows if not n]
        nested_rows = [(c, rj) for (c, n, rj) in rows if n]
        check(any("echo top" in c for c, _ in top_rows),
              f"case5: a TOP-LEVEL brushprov row exists (top={[c for c,_ in top_rows]!r})")
        # Find the nested 'echo nested > /root/nested.txt' record; the
        # parsed-structure JSON must reflect brush's view (1 stage, the literal
        # output target).
        import json as _json
        nested_hit = [(c, rj) for c, rj in nested_rows
                      if c.strip().startswith("echo nested")]
        check(bool(nested_hit),
              f"case1: a NESTED brushprov row exists for the recipe "
              f"(nested={[c for c,_ in nested_rows]!r})")
        if nested_hit:
            rj = _json.loads(nested_hit[0][1])
            check(rj.get("stages") == 1 and "/root/nested.txt" in (rj.get("out_targets") or []),
                  f"case1: nested record has parsed structure brush saw "
                  f"(stages=1, out_targets includes /root/nested.txt): {rj!r}")

        # ── CASE 2: builtins really run THROUGH brush ───────────────────────
        r = subprocess.run(
            [str(BIN), "run", "-b", "VARS", "--",
             "sh", "-c",
             "/bin/sh -c 'X=1; cd /tmp; export Y=2; echo $X-$Y > /root/vars.txt'"],
            capture_output=True, text=True, timeout=120)
        check(r.returncode == 0,
              f"case2: vars/builtins box exits 0 (got {r.returncode}: "
              f"{r.stderr[-300:]})")
        sp2 = latest_sqlar(m)
        check(m.sqlar_content(sp2, "root/vars.txt") == b"1-2\n",
              "case2: vars.txt is '1-2' — assignment, cd, export, $-expansion "
              "all ran THROUGH brush (not silently dropped)")
        con = sqlite3.connect(f"file:{sp2}?mode=ro", uri=True)
        try:
            ncmds = [c for (c,) in con.execute(
                "SELECT cmd FROM brushprov WHERE nested=1")]
        finally:
            con.close()
        check(any("X=1" in c or "echo $X" in c for c in ncmds),
              f"case2: a nested brushprov row exists for the vars recipe "
              f"({ncmds!r})")

        # ── CASE 3: visible failure / no fallback ──────────────────────────
        # Process substitution `<(...)` is genuinely unparseable by brush-core's
        # sh-mode parser. With brush-IS-the-shell this MUST surface as a
        # non-zero exit and a stderr message — never fall back to /bin/sh.
        r = subprocess.run(
            [str(BIN), "run", "-b", "FAIL", "--",
             "sh", "-c",
             # Quote so the OUTER -b body (which is also brush) accepts it; the
             # offending construct must reach the NESTED brush-sh shim.
             "/bin/sh -c 'cat <(echo bad) > /root/bad.txt'"],
            capture_output=True, text=True, timeout=120)
        check(r.returncode != 0,
              f"case3: nested unsupported construct → non-zero exit "
              f"(got {r.returncode})")
        check("NO /bin/sh fallback" in r.stderr or "cannot parse" in r.stderr,
              f"case3: stderr says brush refused (no fallback): "
              f"{r.stderr[-400:]!r}")
        # bad.txt must NOT exist — if a real /bin/sh had fallen in, it would.
        sp3 = latest_sqlar(m)
        try:
            content = m.sqlar_content(sp3, "root/bad.txt")
        except Exception:
            content = None
        check(content in (None, b""),
              f"case3: bad.txt is NOT written (no real-shell fallback): "
              f"content={content!r}")

        # Helpers shared by the in-process-builtin cases below.
        def outputs_of(sp):
            con = sqlite3.connect(f"file:{sp}?mode=ro", uri=True)
            try:
                return b"".join(c for (c,) in con.execute(
                    "SELECT content FROM outputs WHERE stream=0 "
                    "AND content IS NOT NULL"))
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
            # True if the process table shows an external coreutil binary for
            # `util` (e.g. /usr/bin/printf, /bin/cat). The basename of exe (or
            # argv[0]) equals util.
            for exe, argv in process_rows(sp):
                base = os.path.basename(exe)
                if base == util:
                    return True
                # argv is stored as JSON or NUL/space-joined; check first token.
                try:
                    a0 = json.loads(argv)[0] if argv.strip().startswith("[") else argv.split()[0]
                except Exception:
                    a0 = argv.split()[0] if argv.split() else ""
                if os.path.basename(a0) == util:
                    return True
            return False

        # ── CASE 6: echo/printf are IN-PROCESS builtins AND captured ───────
        # printf is a brush BashMode builtin → runs in-process, writes fd 1 →
        # the FUSE sink → captured. No external /usr/bin/printf is forked.
        r = subprocess.run(
            [str(BIN), "run", "-b", "PRINTF", "--",
             "sh", "-c", r'printf "%s-%d\n" hi 42'],
            capture_output=True, text=True, timeout=120)
        check(r.returncode == 0,
              f"case6: printf box exits 0 (got {r.returncode}: {r.stderr[-300:]})")
        sp6 = latest_sqlar(m)
        check(b"hi-42" in outputs_of(sp6),
              f"case6: printf output 'hi-42' captured ({outputs_of(sp6)[:40]!r})")
        check(not has_external(sp6, "printf"),
              "case6: NO external /usr/bin/printf process row (it ran in-process)")
        check(not has_external(sp6, "echo"),
              "case6: NO external echo process row")

        # ── CASE 7: cat is an IN-PROCESS coreutil AND captured ─────────────
        r = subprocess.run(
            [str(BIN), "run", "-b", "CAT", "--",
             "sh", "-c", "echo data > /root/f.txt; cat /root/f.txt"],
            capture_output=True, text=True, timeout=120)
        check(r.returncode == 0,
              f"case7: cat box exits 0 (got {r.returncode}: {r.stderr[-300:]})")
        sp7 = latest_sqlar(m)
        check(m.sqlar_content(sp7, "root/f.txt") == b"data\n",
              "case7: write 'data' captured")
        check(b"data" in outputs_of(sp7),
              f"case7: cat output 'data' captured ({outputs_of(sp7)[:40]!r})")
        check(not has_external(sp7, "cat"),
              "case7: NO external /usr/bin/cat process row (coreutil ran in-process)")

        # ── CASE 8: coreutil in a PIPE — the fd-redirect trap ──────────────
        # `printf … | sort`: sort's stdout is a PIPE stage (not fd 1). The
        # wrapper must dup2 the pipe fd onto the process's real fd 1 around the
        # uumain call, else output goes to the wrong place / is empty.
        r = subprocess.run(
            [str(BIN), "run", "-b", "SORT", "--",
             "sh", "-c", r'printf "c\nb\na\n" | sort'],
            capture_output=True, text=True, timeout=120)
        check(r.returncode == 0,
              f"case8: sort-pipe box exits 0 (got {r.returncode}: {r.stderr[-300:]})")
        sp8 = latest_sqlar(m)
        check(b"a\nb\nc\n" in outputs_of(sp8),
              f"case8: piped `sort` output is a\\nb\\nc\\n — fd-redirect works "
              f"({outputs_of(sp8)[:40]!r})")
        check(not has_external(sp8, "sort"),
              "case8: NO external /usr/bin/sort process row (coreutil in-process)")

        # ── CASE 9: bash-mode bashism works as `bash`, FAILS as `sh` ───────
        # Invoked as bash → BASH-mode parser → `[[ … ]]` works.
        r = subprocess.run(
            [str(BIN), "run", "-b", "BASHOK", "--",
             "sh", "-c", "/bin/bash -c '[[ -d /root ]] && echo yes > /root/bash.txt'"],
            capture_output=True, text=True, timeout=120)
        check(r.returncode == 0,
              f"case9: bash -c [[ ]] box exits 0 (got {r.returncode}: {r.stderr[-300:]})")
        sp9 = latest_sqlar(m)
        check(m.sqlar_content(sp9, "root/bash.txt") == b"yes\n",
              "case9: bash.txt is 'yes' — bash-mode `[[ ]]` ran (mode by name)")
        # The SAME under sh -c (POSIX) must FAIL visibly: `[[` is not a command.
        r = subprocess.run(
            [str(BIN), "run", "-b", "SHBAD", "--",
             "sh", "-c", "/bin/sh -c '[[ -d /root ]] && echo no > /root/shbad.txt'"],
            capture_output=True, text=True, timeout=120)
        check(r.returncode != 0,
              f"case9: sh -c [[ ]] FAILS (POSIX sh-mode, got {r.returncode})")
        sp9b = latest_sqlar(m)
        try:
            shbad = m.sqlar_content(sp9b, "root/shbad.txt")
        except Exception:
            shbad = None
        check(shbad in (None, b""),
              f"case9: shbad.txt NOT written under sh-mode (content={shbad!r})")

        # ── CASE 4 (negative): non-brush box ───────────────────────────────
        # No -b → no shadow binds, no SARUN_BRUSH_SH. The nested /bin/sh is the
        # real system shell directly, and the box has NO brushprov rows.
        r = subprocess.run(
            [str(BIN), "run", "NEG", "--",
             "sh", "-c", "/bin/sh -c 'echo neg > /root/neg.txt'"],
            capture_output=True, text=True, timeout=120)
        check(r.returncode == 0,
              f"case4: non-brush box exits 0 (got {r.returncode}: "
              f"{r.stderr[-300:]})")
        spn = latest_sqlar(m)
        check(m.sqlar_content(spn, "root/neg.txt") == b"neg\n",
              "case4: non-brush nested write IS captured (FUSE)")
        con = sqlite3.connect(f"file:{spn}?mode=ro", uri=True)
        try:
            ncmds = [c for (c,) in con.execute("SELECT cmd FROM brushprov")]
        finally:
            con.close()
        check(ncmds == [],
              f"case4: non-brush box has NO brushprov rows — its /bin/sh is "
              f"NOT intercepted (cmds={ncmds!r})")
    finally:
        for h in host_paths: h.unlink(missing_ok=True)
        if eng is not None and eng.poll() is None:
            eng.terminate()
            try: eng.wait(timeout=10)
            except Exception: eng.kill()
        os.environ.pop("SLOPBOX_NS", None)
        shutil.rmtree(tmp, ignore_errors=True)
    print("\n" + ("BRUSH-NESTED-SH-RS PASS" if not _fails
                  else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_brush_nested_sh_rs():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
