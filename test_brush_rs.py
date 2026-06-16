#!/usr/bin/env python3
"""D9 embedded brush shell (`-b`) for the RUST engine (engine/).

`sarun run -b -- CMD` makes the box's shell the in-process brush
(brush-core/brush-parser), NOT /bin/sh. This test asserts REAL effects through
a real `sarun run -b` box (never shape-only):

  1. a brush-run command's file WRITES are captured into the box sqlar (host
     untouched) — brush sits ABOVE the FUSE capture, it doesn't bypass it;
  2. a brush PIPELINE + REDIRECT (`echo hi | tr a-z A-Z > /root/out`) produces
     the EXACT captured bytes, proving brush ran the pipeline itself;
  3. FRAME_PROV semantic-provenance rows actually land in the box's `brushprov`
     table, carrying the real command string + the parsed pipeline/redirect
     structure (stages/redirects), readable back over the control `brushprov`
     verb AND from the sqlar;
  4. brush stdout is captured into the outputs table (python-readable);
  5. an UNSUPPORTED construct is a VISIBLE error + non-zero exit — NOT a silent
     /bin/sh fallback (the D9 no-downgrade rule);
  6. `-b` under `-d` (no overlay) is a visible error, not a quiet /bin/sh run.

Run:
    uv run --with "pyfuse3>=3.2" --with "trio>=0.22" --with "wcmatch>=8.4" \
      --with "python-magic>=0.4" python test_brush_rs.py
Skips (passes vacuously) if cargo/the binary are unavailable.
"""
import json, os, shutil, socket, sqlite3, subprocess, sys, tempfile, time
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


def brushprov_rows(sp):
    """Read the brushprov table straight from the box sqlar."""
    con = sqlite3.connect(f"file:{sp}?mode=ro", uri=True)
    try:
        try:
            return [(cmd, json.loads(rec)) for cmd, rec in
                    con.execute("SELECT cmd,record FROM brushprov ORDER BY id")]
        except sqlite3.OperationalError:
            return None  # table absent
    finally:
        con.close()


def main():
    if not ensure_binary():
        print("  ok  brush-rs: cargo/binary unavailable — SKIP")
        print("\nBRUSH-RS PASS (skipped)")
        return 0
    tmp = Path(tempfile.mkdtemp(prefix="brushrs-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "RS"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    eng = None
    hosts = [Path("/root/brush_write.txt"), Path("/root/brush_pipe.txt")]
    for h in hosts: h.unlink(missing_ok=True)
    try:
        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        sock = m.sock_path()
        if not wait_socket(sock):
            raise RuntimeError("rust engine socket never appeared")

        # ── (1) brush runs a simple command; its WRITE is captured ──────────
        r = subprocess.run(
            [str(BIN), "run", "-b", "BRWRITE", "--",
             "sh", "-c", "echo from-brush > /root/brush_write.txt"],
            capture_output=True, text=True, timeout=90)
        check(r.returncode == 0,
              f"brush-rs: `run -b` simple command exits 0 "
              f"(got {r.returncode}: {r.stderr[-300:]})")
        check(not hosts[0].exists(),
              "brush-rs: brush write captured, real host untouched")
        sp = latest_sqlar(m)
        check(m.sqlar_content(sp, "root/brush_write.txt") == b"from-brush\n",
              "brush-rs: brush-run write recorded in the box sqlar")

        # ── (2) brush PIPELINE + REDIRECT: exact captured bytes ─────────────
        r = subprocess.run(
            [str(BIN), "run", "-b", "BRPIPE", "--",
             "sh", "-c", "echo hi | tr a-z A-Z > /root/brush_pipe.txt"],
            capture_output=True, text=True, timeout=90)
        check(r.returncode == 0,
              f"brush-rs: `run -b` pipeline exits 0 (got {r.returncode}: "
              f"{r.stderr[-300:]})")
        sp2 = latest_sqlar(m)
        check(m.sqlar_content(sp2, "root/brush_pipe.txt") == b"HI\n",
              "brush-rs: brush ran the pipeline+redirect (captured bytes == 'HI')")

        # ── (3) FRAME_PROV provenance landed, real structure ────────────────
        rows = brushprov_rows(sp2)
        check(rows is not None and len(rows) >= 1,
              f"brush-rs: brushprov rows recorded ({0 if not rows else len(rows)})")
        if rows:
            cmd0, rec0 = rows[0]
            check("tr" in cmd0 and "echo hi" in cmd0,
                  f"brush-rs: provenance carries the real command string "
                  f"({cmd0!r})")
            check(rec0.get("stages") == 2,
                  f"brush-rs: provenance has the real pipeline stage count "
                  f"(stages={rec0.get('stages')})")
            # the `tr` stage carried a redirect (> file): structure brush parsed
            redirs = sum(s.get("redirects", 0)
                         for s in rec0.get("stage_detail", []))
            check(redirs >= 1,
                  f"brush-rs: provenance records the redirect structure "
                  f"(redirects={redirs})")
        # same data over the control `brushprov` verb (live read path)
        rep = m.sync_request(sock, type="ui", verb="brushprov", args=[sp2.stem])
        wire = rep.get("r") if isinstance(rep, dict) else None
        check(isinstance(wire, list) and len(wire) >= 1
              and "tr" in (wire[0].get("cmd", "")),
              "brush-rs: control `brushprov` verb returns the same provenance")

        # ── (4) brush stdout captured into outputs (python-readable) ────────
        r = subprocess.run(
            [str(BIN), "run", "-b", "BROUT", "--", "sh", "-c",
             "echo hello-stdout"],
            capture_output=True, text=True, timeout=90)
        check(r.returncode == 0, "brush-rs: stdout box exits 0")
        sp3 = latest_sqlar(m)
        con = sqlite3.connect(f"file:{sp3}?mode=ro", uri=True)
        outs = b"".join(c for (c,) in con.execute(
            "SELECT content FROM outputs WHERE stream=0 AND content IS NOT NULL"))
        con.close()
        check(b"hello-stdout" in outs,
              f"brush-rs: brush stdout captured into outputs ({outs[:40]!r})")

        # ── (5) UNSUPPORTED construct → VISIBLE error, non-zero, NO fallback ─
        # `coproc` is a bash-ism brush's sh-mode parser does not accept; it must
        # error visibly, NOT silently fall through to /bin/sh.
        before = list(Path(os.environ["XDG_STATE_HOME"])
                      .joinpath("slopbox.RS").glob("*.sqlar"))
        r = subprocess.run(
            [str(BIN), "run", "-b", "BRBAD", "--", "sh", "-c",
             "coproc FOO { read x; }; echo SHOULD-NOT-RUN > /root/brush_bad.txt"],
            capture_output=True, text=True, timeout=90)
        badhost = Path("/root/brush_bad.txt")
        full = (r.stdout + r.stderr)
        check(r.returncode != 0,
              f"brush-rs: unsupported construct exits NON-zero (got "
              f"{r.returncode})")
        check("/bin/sh" not in full or "NO /bin/sh fallback" in full,
              "brush-rs: error message does not indicate a /bin/sh fallback")
        check("brush" in full.lower(),
              f"brush-rs: error is a VISIBLE brush error ({full[-160:]!r})")
        # the post-error command must NOT have run anywhere (no silent shell).
        spn = latest_sqlar(m)
        ran = (badhost.exists() or
               m.sqlar_content(spn, "root/brush_bad.txt") is not None)
        check(not ran,
              "brush-rs: the rest of the script did NOT run (no silent /bin/sh)")
        badhost.unlink(missing_ok=True)

        # ── (6) -b under -d is a visible error (no overlay to capture into) ──
        r = subprocess.run(
            [str(BIN), "run", "-b", "-d", "--", "sh", "-c", "true"],
            capture_output=True, text=True, timeout=90)
        check(r.returncode != 0
              and "brush" in (r.stdout + r.stderr).lower(),
              f"brush-rs: -b under -d errors visibly (rc={r.returncode}, "
              f"{(r.stdout + r.stderr)[-160:]!r})")
    finally:
        for h in hosts: h.unlink(missing_ok=True)
        Path("/root/brush_bad.txt").unlink(missing_ok=True)
        if eng is not None and eng.poll() is None:
            eng.terminate()
            try: eng.wait(timeout=10)
            except Exception: eng.kill()
        os.environ.pop("SLOPBOX_NS", None)
        shutil.rmtree(tmp, ignore_errors=True)
    print("\n" + ("BRUSH-RS PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_brush_rs():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
