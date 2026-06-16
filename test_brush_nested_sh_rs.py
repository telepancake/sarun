#!/usr/bin/env python3
"""D9 follow-on — NESTED-shell semantic provenance for the RUST engine (engine/).

The gap this closes: today brush sees only a -b box's TOP-LEVEL command. When
that command spawns a NESTED shell — `make` (or libc system(), or any tool)
exec'ing `/bin/sh -c "RECIPE"` — the recipe's PROCESSES are already attributed
to the top-level pipeline by forest ancestry, but the recipe's OWN semantic
command string got NO brushprov row. This wires an OBSERVE-ONLY interposition:
runner shadows the box's /bin/sh (etc.) with the engine binary; the brush-sh
shim parses the nested `-c` script, emits its provenance to the engine over a
`brush_prov_nested` control message, then execve's the REAL shell with the
ORIGINAL argv — so the recipe runs byte-for-byte unchanged.

REAL effects, never shape-only:
  1. A -b box whose top-level command spawns a nested `/bin/sh -c "echo nested
     > /root/nested.txt"` (alongside a top-level `echo top > /root/top.txt`).
     We assert BOTH writes are captured, AND a brushprov row exists for the
     NESTED recipe (`echo nested ...`) flagged nested=1, IN ADDITION to the
     top-level rows — read via the sqlar AND the `brushprov` control verb.
  2. The recipe ran via the REAL shell: the captured file content is exactly
     right (interception did not change behavior).
  3. Negative: a NON-brush box's `/bin/sh -c` is NOT intercepted — no nested
     brushprov rows (no shadow binds, no setenv for a non-brush box).

Run:
    uv run --with "pyfuse3>=3.2" --with "trio>=0.22" --with "wcmatch>=8.4" \
      --with "python-magic>=0.4" python test_brush_nested_sh_rs.py
Skips (passes vacuously) if cargo/the binary are unavailable.
"""
import os, shutil, socket, sqlite3, subprocess, sys, tempfile, time
from pathlib import Path
from importlib.machinery import SourceFileLoader

_HERE = Path(__file__).resolve().parent
SARUN = str(_HERE / "sarun")
CRATE = _HERE / "engine"
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


def main():
    if not ensure_binary():
        print("  ok  brush-nested-sh-rs: cargo/binary unavailable — SKIP")
        print("\nBRUSH-NESTED-SH-RS PASS (skipped)")
        return 0
    tmp = Path(tempfile.mkdtemp(prefix="brushnestrs-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "RS"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    eng = None
    # Host paths that must never be touched (the box captures into the overlay).
    host_top = Path("/root/top.txt")
    host_nested = Path("/root/nested.txt")
    host_neg = Path("/root/neg.txt")
    for h in (host_top, host_nested, host_neg): h.unlink(missing_ok=True)
    try:
        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        sock = m.sock_path()
        if not wait_socket(sock):
            raise RuntimeError("rust engine socket never appeared")

        # ── PARTS 1 & 2: top-level + nested recipe, observe-only ────────────
        # The top-level command (run by the box's embedded brush) writes top.txt
        # AND spawns a NESTED `/bin/sh -c` (the make-recipe shape) that writes
        # nested.txt. The nested sh is shadowed by the engine binary (brush-sh
        # shim): it emits the recipe's provenance, then execs the REAL /bin/sh so
        # the recipe runs unchanged.
        r = subprocess.run(
            [str(BIN), "run", "-b", "NEST", "--",
             "sh", "-c",
             "echo top > /root/top.txt; "
             "/bin/sh -c 'echo nested > /root/nested.txt'"],
            capture_output=True, text=True, timeout=120)
        check(r.returncode == 0,
              f"brush-nested-sh-rs: box exits 0 (got {r.returncode}: "
              f"{r.stderr[-400:]})")
        check(not host_top.exists() and not host_nested.exists(),
              "brush-nested-sh-rs: both writes captured, real host untouched")

        sp = latest_sqlar(m)
        # PART 2: the recipe really ran via the REAL shell — content is correct.
        check(m.sqlar_content(sp, "root/top.txt") == b"top\n",
              "brush-nested-sh-rs: top-level write captured ('top')")
        check(m.sqlar_content(sp, "root/nested.txt") == b"nested\n",
              "brush-nested-sh-rs: NESTED recipe ran via real shell, write "
              "captured ('nested') — behavior unchanged")

        # PART 1: a brushprov row for the NESTED recipe, flagged nested=1, IN
        # ADDITION to the top-level row(s). Read the sqlar directly.
        con = sqlite3.connect(f"file:{sp}?mode=ro", uri=True)
        try:
            rows = con.execute(
                "SELECT cmd, nested FROM brushprov ORDER BY id").fetchall()
        finally:
            con.close()
        top_rows = [c for (c, n) in rows if not n]
        nested_rows = [c for (c, n) in rows if n]
        check(any("echo top" in c for c in top_rows),
              f"brush-nested-sh-rs: a TOP-LEVEL brushprov row exists "
              f"(top_rows={top_rows!r})")
        check(any(c.strip().startswith("echo nested") for c in nested_rows),
              f"brush-nested-sh-rs: a NESTED brushprov row exists for the "
              f"recipe, flagged nested=1 (nested_rows={nested_rows!r})")

        # Same via the control `brushprov` verb (live read path): the nested row
        # is present AND carries nested:true.
        rep = m.sync_request(sock, type="ui", verb="brushprov", args=[sp.stem])
        bprows = rep.get("r") if isinstance(rep, dict) else None
        nested_via_verb = [b for b in (bprows or [])
                           if b.get("nested") is True
                           and b.get("cmd", "").strip().startswith("echo nested")]
        check(bool(nested_via_verb),
              f"brush-nested-sh-rs: control brushprov verb reports the nested "
              f"row with nested:true ({bprows!r})")
        # And a top-level row that is NOT flagged nested.
        check(any(b.get("nested") is False and "echo top" in b.get("cmd", "")
                  for b in (bprows or [])),
              "brush-nested-sh-rs: control verb still reports top-level rows "
              "as nested:false")

        # ── PART 3 (negative): a NON-brush box is NOT intercepted ───────────
        # Without -b there are no shadow binds, no SARUN_BRUSH_SH, so the nested
        # /bin/sh is the real shell directly — and there is no brushprov table
        # content at all (non-brush boxes never emit provenance).
        r = subprocess.run(
            [str(BIN), "run", "NEG", "--",
             "sh", "-c", "/bin/sh -c 'echo neg > /root/neg.txt'"],
            capture_output=True, text=True, timeout=120)
        check(r.returncode == 0,
              f"brush-nested-sh-rs: non-brush box exits 0 (got {r.returncode}: "
              f"{r.stderr[-300:]})")
        spn = latest_sqlar(m)
        check(m.sqlar_content(spn, "root/neg.txt") == b"neg\n",
              "brush-nested-sh-rs: non-brush nested write IS captured (FUSE)")
        con = sqlite3.connect(f"file:{spn}?mode=ro", uri=True)
        try:
            ncmds = [c for (c,) in con.execute("SELECT cmd FROM brushprov")]
        finally:
            con.close()
        check(ncmds == [],
              f"brush-nested-sh-rs: non-brush box has NO brushprov rows — its "
              f"/bin/sh is NOT intercepted (cmds={ncmds!r})")
    finally:
        for h in (host_top, host_nested, host_neg): h.unlink(missing_ok=True)
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
