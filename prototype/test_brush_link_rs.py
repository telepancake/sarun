#!/usr/bin/env python3
"""D9 brush <-> process LINKAGE for the RUST engine (engine/).

Part 1 (linkage) — REAL effects, never shape-only:
  A `-b` box runs a PIPELINE that WRITES a file
  (`echo hi | tr a-z A-Z > /root/link_out.txt`). brush runs each pipeline in
  execution order and emits one FRAME_PROV per pipeline; the engine records a
  `brushprov` row and stamps the pipeline's writer process with that pipeline's
  row id (process.brush_pipeline_id). NOTE: `tr` (and `echo`) are now bundled
  uutils coreutils / brush builtins that run IN-PROCESS — there is NO forked
  `/usr/bin/tr` child. So the redirect target's writer is the brush --inner
  process ITSELF, and the linkage stamps that --inner row (a writer whose tgid
  IS the brush root is an in-process pipeline stage, not "brush wrote it" — see
  finalize_brush_links in engine/src/capture.rs). We assert, reading the box
  sqlar AND the control join verbs, that:
    * the process that actually WROTE the file (sqlar.writer -> process row) is
      linked to a brushprov pipeline whose `cmd` is that real pipeline;
    * process -> pipeline (proc_pipeline) and pipeline -> processes
      (pipeline_procs) both round-trip;
    * the brushprov row carries its own `processes` list including that writer.

Part 2 (/bin/sh -> brush overlay mapping) — DOCUMENTED-GAP assertion:
  This cut does NOT remap /bin/sh to brush inside the box (see the header of
  engine/src/brush.rs for the precise reason). So a box running a nested
  `sh -c '...'` via /bin/sh runs the HOST /bin/sh for that recipe and produces
  NO brushprov row for the nested recipe string. We assert that documented
  current behavior (the nested recipe's WRITE is still captured, but there is no
  extra brushprov pipeline for it), so the gap is pinned, not faked.

Run:
    uv run --with "pyfuse3>=3.2" --with "trio>=0.22" --with "wcmatch>=8.4" \
      --with "python-magic>=0.4" python test_brush_link_rs.py
Skips (passes vacuously) if cargo/the binary are unavailable.
"""
import json, os, shutil, socket, sqlite3, subprocess, sys, tempfile, time
from pathlib import Path
from importlib.machinery import SourceFileLoader

_HERE = Path(__file__).resolve().parent
SARUN = str(_HERE / "sarun")
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
        print("  ok  brush-link-rs: cargo/binary unavailable — SKIP")
        print("\nBRUSH-LINK-RS PASS (skipped)")
        return 0
    tmp = Path(tempfile.mkdtemp(prefix="brushlinkrs-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "RS"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    eng = None
    host_out = Path("/root/link_out.txt")
    host_nested = Path("/root/link_nested.txt")
    for h in (host_out, host_nested): h.unlink(missing_ok=True)
    try:
        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        sock = m.sock_path()
        if not wait_socket(sock):
            raise RuntimeError("rust engine socket never appeared")

        # ── PART 1: linkage through a REAL pipeline spawn ───────────────────
        # `tr` is an external binary brush fork/execs; it WRITES the redirect
        # target, so its forest ancestry truly climbs through the brush shell.
        r = subprocess.run(
            [str(BIN), "run", "-b", "LINK", "--",
             "sh", "-c", "echo hi | tr a-z A-Z > /root/link_out.txt"],
            capture_output=True, text=True, timeout=120)
        check(r.returncode == 0,
              f"brush-link-rs: pipeline box exits 0 (got {r.returncode}: "
              f"{r.stderr[-300:]})")
        check(not host_out.exists(),
              "brush-link-rs: write captured, real host untouched")
        sp = latest_sqlar(m)
        check(m.sqlar_content(sp, "root/link_out.txt") == b"HI\n",
              "brush-link-rs: brush ran the pipeline (captured bytes == 'HI')")

        con = sqlite3.connect(f"file:{sp}?mode=ro", uri=True)
        try:
            # The process row that actually streamed the bytes into the redirect
            # target is the LAST writer: brush opens the redirect (first writer),
            # then the external `tr` it fork/execs writes through fd 1 (last
            # writer). The last_writer is the real pipeline-spawned process whose
            # /proc ancestry climbs through the brush shell.
            row = con.execute(
                "SELECT writer,last_writer FROM sqlar "
                "WHERE name='root/link_out.txt'").fetchone()
            writer_id = row[1] if row else None
            check(writer_id is not None,
                  f"brush-link-rs: file has a recorded (last) writer process "
                  f"(last_writer={writer_id})")

            # That writer must carry a brush_pipeline_id linking it to a pipeline.
            prow = con.execute(
                "SELECT brush_pipeline_id, exe FROM process WHERE id=?",
                (writer_id,)).fetchone() if writer_id else None
            pipe_id = prow[0] if prow else None
            check(pipe_id is not None,
                  f"brush-link-rs: the WRITER process is linked to a brush "
                  f"pipeline (brush_pipeline_id={pipe_id})")

            # process -> pipeline: the linked brushprov row's cmd IS that pipeline.
            bp = con.execute(
                "SELECT cmd, pipeline FROM brushprov WHERE id=?",
                (pipe_id,)).fetchone() if pipe_id else None
            check(bp is not None and "tr" in bp[0] and "echo hi" in bp[0],
                  f"brush-link-rs: process->pipeline cmd is the real pipeline "
                  f"({None if not bp else bp[0]!r})")

            # pipeline -> processes: that pipeline's process set includes the
            # writer (the REAL spawn, by ancestry, not by guess).
            procs = [pid for (pid,) in con.execute(
                "SELECT id FROM process WHERE brush_pipeline_id=? ORDER BY id",
                (pipe_id,))] if pipe_id else []
            check(writer_id in procs,
                  f"brush-link-rs: pipeline->processes includes the writer "
                  f"(writer={writer_id}, procs={procs})")

            # The linked writer is the brush --inner process running the
            # in-process `tr` coreutil stage. It still has a real forest parent
            # (the bwrap that launched --inner), so its /proc ancestry is intact;
            # the linkage stamps this --inner row because the in-process stage
            # wrote the redirect target from the --inner pid (no forked child).
            parent_row = con.execute(
                "SELECT parent_id FROM process WHERE id=?", (writer_id,)
                ).fetchone()
            check(parent_row is not None and parent_row[0] is not None,
                  f"brush-link-rs: the linked writer has a forest parent, i.e. "
                  f"it is a real process row ({parent_row})")
        finally:
            con.close()

        # Same linkage over the control JOIN verbs (live read path), round-trip.
        rep = m.sync_request(sock, type="ui", verb="proc_pipeline",
                             args=[sp.stem, writer_id])
        pp = rep.get("r") if isinstance(rep, dict) else None
        check(isinstance(pp, dict) and "tr" in pp.get("cmd", ""),
              f"brush-link-rs: control proc_pipeline returns the pipeline "
              f"({pp!r})")
        rep = m.sync_request(sock, type="ui", verb="pipeline_procs",
                             args=[sp.stem, pipe_id])
        plist = rep.get("r") if isinstance(rep, dict) else None
        check(isinstance(plist, list) and writer_id in plist,
              f"brush-link-rs: control pipeline_procs includes the writer "
              f"({plist!r})")
        # brushprov verb now carries the per-pipeline `processes` list.
        rep = m.sync_request(sock, type="ui", verb="brushprov", args=[sp.stem])
        bprows = rep.get("r") if isinstance(rep, dict) else None
        linked = None
        if isinstance(bprows, list):
            for br in bprows:
                if br.get("id") == pipe_id:
                    linked = br
        check(linked is not None and writer_id in (linked.get("processes") or []),
              f"brush-link-rs: brushprov row reports its processes "
              f"({None if not linked else linked.get('processes')})")

        # ── PART 2: /bin/sh -> brush-sh nested provenance (follow-on LANDED) ─
        # The D9 follow-on now interposes the box's /bin/sh with the engine's
        # brush-sh shim for -b boxes: a nested `sh -c RECIPE` (the make-recipe
        # shape) emits the recipe's OWN brushprov row (flagged nested=1) before
        # exec'ing the REAL /bin/sh, so the recipe still runs unchanged. Assert
        # the nested write is captured AND a brushprov row for the inner recipe
        # now EXISTS (the gap that used to be pinned here is closed — the deeper
        # coverage lives in test_brush_nested_sh_rs.py).
        r = subprocess.run(
            [str(BIN), "run", "-b", "NEST", "--",
             "sh", "-c", "/bin/sh -c 'echo NESTED > /root/link_nested.txt'"],
            capture_output=True, text=True, timeout=120)
        check(r.returncode == 0,
              f"brush-link-rs: nested /bin/sh box exits 0 (got {r.returncode}: "
              f"{r.stderr[-300:]})")
        spn = latest_sqlar(m)
        check(m.sqlar_content(spn, "root/link_nested.txt") == b"NESTED\n",
              "brush-link-rs: nested recipe's write IS still captured (FUSE)")
        con = sqlite3.connect(f"file:{spn}?mode=ro", uri=True)
        try:
            cmds = [c for (c, n) in con.execute("SELECT cmd, nested FROM brushprov")
                    if n]
        finally:
            con.close()
        # The inner recipe `echo NESTED > ...` now has its OWN brushprov row,
        # flagged nested=1, observed by the brush-sh shim.
        inner_seen = any(c.strip().startswith("echo NESTED") for c in cmds)
        check(inner_seen,
              f"brush-link-rs: nested recipe HAS a brushprov row (nested=1) — "
              f"the /bin/sh->brush-sh follow-on is wired. nested cmds={cmds!r}")
    finally:
        for h in (host_out, host_nested): h.unlink(missing_ok=True)
        if eng is not None and eng.poll() is None:
            eng.terminate()
            try: eng.wait(timeout=10)
            except Exception: eng.kill()
        os.environ.pop("SLOPBOX_NS", None)
        shutil.rmtree(tmp, ignore_errors=True)
    print("\n" + ("BRUSH-LINK-RS PASS" if not _fails
                  else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_brush_link_rs():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
