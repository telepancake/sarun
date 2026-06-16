#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = [
#   "pyfuse3>=3.2", "trio>=0.22", "wcmatch>=8.4", "python-magic>=0.4",
# ]
# ///
"""REAL process-FOREST conformance for the RUST engine (engine/src/capture.rs).

The Rust capture must build the SAME connected process FOREST the Python engine
does: each writing process's row carries a `parent_id` that is the ROW id of its
parent process's incarnation, chained up the PPid ladder to a ROOT (the box's
`sarun -- cmd` runner, root=1). Process identity is (tgid,start) — field 22 of
/proc/<pid>/stat — so a reused pid with a new start_time is a NEW row.

This test runs a REAL box (Rust `serve` engine + a real `sarun NAME -- cmd`
runner) whose command spawns a KNOWN multi-level process tree that writes files
at several depths, then reads the box's on-disk sqlar process table and asserts
the recorded `parent_id` chain reconstructs the ACTUAL ancestry:

  * every recorded process row reaches a root=1 row by following parent_id
    (one connected forest — no orphan with parent_id NULL except the roots);
  * the writers at depths 1/2/3 form a real parent->child->grandchild chain via
    parent_id (NOT a flat list of NULL parents — the old, broken behaviour);
  * `start` is the REAL /proc start_time (never 0) for in-box incarnations;
  * a RERUN of the same NAME adds a SECOND root, and the forest stays connected.

It is NOT a shape test: the asserted edges are checked against the real spawn
ancestry the workload created (the exe/argv of each level), and cross-checked
against what the Python `process_list`/`process_roots` readers yield for the
same sqlar (the very readers the review UI uses).

    ./test_proc_forest_rs.py        # uv installs deps, builds the engine if needed
Skips (passes vacuously) if cargo/the binary or FUSE/bwrap are unavailable.
"""
import json, os, socket, subprocess, sys, tempfile, shutil, time, signal
from pathlib import Path
from importlib.machinery import SourceFileLoader

_HERE = Path(__file__).resolve().parent
SARUN = str(_HERE / "sarun")
PYBIN = sys.executable
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


# A KNOWN 3-level process tree. Each level is a distinct `sh` process that writes
# its OWN file then (for the upper two) exec-spawns the next level and waits, so
# all three are alive while the deeper writes happen — the bubble walk can /proc
# each ancestor. The marker files name their depth so we can map writer rows back
# to spawn depth. Each level sleeps briefly AFTER writing so its /proc survives
# the deeper writers' attribution.
WORKLOAD = r"""
set -e
echo d1 > /root/depth1.txt
sh -c '
  echo d2 > /root/depth2.txt
  sh -c "echo d3 > /root/depth3.txt; sleep 0.3"
  sleep 0.3
'
sleep 0.3
"""


def run_box(env, name):
    r = subprocess.run([PYBIN, SARUN, name, "--", "sh", "-c", WORKLOAD],
                       env=env, capture_output=True, text=True, timeout=180)
    if r.returncode != 0:
        raise RuntimeError(f"box run failed ({r.returncode}): {r.stderr[-500:]}")


def forest_check(m, path, label, expect_roots):
    """Read the box's process table via the Python readers and verify the forest."""
    rows = m.process_list(path)            # [(id,tgid,ppid,parent_id,exe,argv)]
    roots = m.process_roots(path)          # {row ids with root=1}
    by_id = {r[0]: r for r in rows}
    check(bool(rows), f"{label}: process table is non-empty ({len(rows)} rows)")
    check(len(roots) == expect_roots,
          f"{label}: forest has {expect_roots} root(s) (got {len(roots)}: {sorted(roots)})")
    check(roots <= set(by_id), f"{label}: every root id is a real process row")

    # 1) CONNECTEDNESS: every row reaches a root by following parent_id; the only
    #    rows allowed a NULL parent_id are the roots themselves.
    orphans, unreachable = [], []
    for rid, _tgid, _ppid, parent_id, _exe, _argv in rows:
        if rid in roots:
            continue
        if parent_id is None:
            orphans.append(rid); continue
        # walk up to a root (cycle/over-length guarded)
        seen, cur, ok = set(), rid, False
        while cur is not None and cur not in seen and len(seen) <= len(rows) + 1:
            seen.add(cur)
            if cur in roots:
                ok = True; break
            cur = by_id[cur][3] if cur in by_id else None
        if not ok:
            unreachable.append(rid)
    check(not orphans,
          f"{label}: no non-root row has a NULL parent_id (orphans={orphans}) "
          f"— this is exactly the OLD flat-list bug")
    check(not unreachable,
          f"{label}: every row's parent_id chain reaches a root (unreachable={unreachable})")

    # 2) Every parent_id points at a row that REALLY exists (a structural edge,
    #    not a dangling tgid masquerading as a row id).
    bad = [(r[0], r[3]) for r in rows if r[3] is not None and r[3] not in by_id]
    check(not bad, f"{label}: every parent_id is a real row id (dangling={bad})")
    return rows, roots, by_id


def depth_writers(m, path, by_id):
    """Map each depthN.txt to the ROW id of the process that wrote it (sqlar
    last_writer), via the Python reader — the same provenance the UI shows."""
    out = {}
    for d in (1, 2, 3):
        wid = m.sqlar_writer_id(path, f"root/depth{d}.txt")
        out[d] = wid
    return out


def main():
    if not ensure_binary():
        print("  ok  proc-forest-rs: cargo/binary unavailable — SKIP")
        print("\nPROC-FOREST-RS PASS (skipped)")
        return 0
    # FUSE/bwrap availability (real box): if missing, skip cleanly.
    if shutil.which("bwrap") is None or not Path("/dev/fuse").exists():
        print("  ok  proc-forest-rs: bwrap/FUSE unavailable — SKIP")
        print("\nPROC-FOREST-RS PASS (skipped)")
        return 0

    tmp = Path(tempfile.mkdtemp(prefix="forestrs-"))
    env = dict(os.environ)
    env["XDG_STATE_HOME"] = str(tmp / "state")
    env["XDG_RUNTIME_DIR"] = str(tmp / "run")
    env["XDG_CONFIG_HOME"] = str(tmp / "config")
    env["XDG_DATA_HOME"] = str(tmp / "data")
    env["SLOPBOX_NS"] = "FOREST"
    env["TEXTUAL"] = ""; env.setdefault("TERM", "dumb")
    for k in ("XDG_STATE_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME", "XDG_DATA_HOME"):
        os.environ[k] = env[k]
    os.environ["SLOPBOX_NS"] = "FOREST"

    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    eng = subprocess.Popen([str(BIN), "serve"],
                           stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    try:
        sock = m.sock_path()
        if not wait_socket(sock):
            out = eng.stdout.read(3000) if eng.stdout else b""
            raise RuntimeError("rust engine socket never appeared:\n"
                               + out.decode(errors="replace"))

        # ── FIRST RUN ───────────────────────────────────────────────────────
        run_box(env, "FBOX")
        # Resolve the box id for FBOX by NAME (the runner auto-creates it).
        rsup = m.RemoteSupervisor(sock)
        sid = next((s for s in rsup.sessions
                    if rsup.display_path(s) == "FBOX"), None)
        check(sid is not None, "proc-forest-rs: FBOX box exists after the run")
        if sid is None:
            raise RuntimeError("FBOX not found")
        path = m.sqlar_path(sid)

        rows, roots, by_id = forest_check(m, path, "first-run", expect_roots=1)

        # start_time must be the REAL /proc value (non-zero) for in-box rows — the
        # whole point of the (tgid,start) incarnation key. Read it straight from
        # the sqlar (process_list drops it).
        import sqlite3
        con = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
        starts = con.execute("SELECT id,tgid,start,root FROM process").fetchall()
        con.close()
        zero_start = [(i, t) for (i, t, s, _r) in starts if (s or 0) == 0]
        check(not zero_start,
              f"proc-forest-rs: every process row has a REAL /proc start_time "
              f"(zero-start rows={zero_start})")

        # ── REAL ANCESTRY: depth1<-depth2<-depth3 writers form a parent chain ──
        wr = depth_writers(m, path, by_id)
        check(all(wr[d] is not None for d in (1, 2, 3)),
              f"proc-forest-rs: all three depth files have a writer row ({wr})")
        if all(wr[d] is not None for d in (1, 2, 3)):
            # The depth-3 writer's parent chain must pass THROUGH the depth-2
            # writer and then the depth-1 writer (the real spawn ancestry the
            # workload created), terminating at a root.
            def chain(rid):
                out, seen = [], set()
                while rid is not None and rid not in seen:
                    seen.add(rid); out.append(rid)
                    rid = by_id[rid][3] if rid in by_id else None
                return out
            c3 = chain(wr[3])
            check(wr[2] in c3,
                  f"proc-forest-rs: depth-3 writer's ancestry passes through the "
                  f"depth-2 writer (chain={c3}, d2={wr[2]})")
            check(wr[1] in c3,
                  f"proc-forest-rs: depth-3 writer's ancestry passes through the "
                  f"depth-1 writer (chain={c3}, d1={wr[1]})")
            # ordering: d2 is an ancestor of d3, d1 an ancestor of d2.
            if wr[2] in c3 and wr[1] in c3:
                check(c3.index(wr[2]) < c3.index(wr[1]),
                      "proc-forest-rs: the chain orders depth3 -> depth2 -> depth1 "
                      f"(chain={c3})")
            check(any(x in roots for x in c3),
                  f"proc-forest-rs: the depth chain terminates at a root (chain={c3}, "
                  f"roots={sorted(roots)})")

        # ── RERUN: a second `FBOX -- cmd` adds ANOTHER root + subtree ──────────
        run_box(env, "FBOX")
        rows2, roots2, by_id2 = forest_check(m, path, "rerun", expect_roots=2)
        check(len(roots2) == 2 and roots <= roots2,
              f"proc-forest-rs: rerun added a SECOND root, keeping the prior one "
              f"(before={sorted(roots)}, after={sorted(roots2)})")
        check(len(by_id2) > len(by_id),
              f"proc-forest-rs: rerun ADDED rows (not a dedup): "
              f"{len(by_id)} -> {len(by_id2)}")
    finally:
        eng.send_signal(signal.SIGTERM)
        try: eng.wait(timeout=20)
        except Exception: eng.kill()
        shutil.rmtree(tmp, ignore_errors=True)

    print("\n" + ("PROC-FOREST-RS PASS" if not _fails
                  else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_proc_forest_rs():
    assert main() == 0


if __name__ == "__main__":
    sys.exit(main())
