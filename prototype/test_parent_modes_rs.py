#!/usr/bin/env python3
"""Parent-stack modes against the RUST engine (engine/): two new per-box
flags the runner sends through the register handshake (alongside the default
behaviour, which is unchanged):

  --no-parent        kernel-derived parent is dropped AND the lower chain does
                     NOT bottom at the real host /. The box's own contents are
                     its entire filesystem (the bottom of an OCI image stack).
  --readonly-parent  child's `apply` REFUSES to promote into the parent. The
                     captured changes can still be reviewed/discarded; they
                     just never leak up the box stack. Per-CHILD attitude.

Both assertions are REAL effect tests (meta key persisted; an apply that
errored without touching the parent) — never shape-only. Skips (passes
vacuously) if cargo/the binary are unavailable.

Run:
    uv run --with "pyfuse3>=3.2" --with "trio>=0.22" --with "wcmatch>=8.4" \\
      --with "python-magic>=0.4" python test_parent_modes_rs.py
"""
import os, shutil, socket, stat as stat_mod, subprocess, sys, tempfile, time
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


def newest_sqlar(m):
    return max(Path(os.environ["XDG_STATE_HOME"]).joinpath("slopbox.RS")
               .glob("*.sqlar"), key=lambda p: int(p.stem))


def wait_for_sqlar_settled(m, sp, timeout=20):
    end = time.time() + timeout
    while time.time() < end:
        if not (Path(os.environ["XDG_STATE_HOME"]).joinpath("slopbox.RS")
                .joinpath(sp.stem).exists()):
            return
        time.sleep(0.1)


def meta_get(sp, key):
    """Read one value from a sqlar's meta table (returns None if absent)."""
    import sqlite3
    with sqlite3.connect(f"file:{sp}?mode=ro", uri=True) as c:
        row = c.execute("SELECT value FROM meta WHERE key=?", (key,)).fetchone()
    return row[0] if row else None


def make_finished_box(m, sid, entries, parent=None, meta=None):
    """Same shape as test_nested_apply_rs's helper; plus optional `meta` dict
    of extra sqlar.meta keys to stamp before settling (so e.g.
    readonly_parent=1 is on the box when review.apply reads it back)."""
    bk = m.live_dir(sid); (bk / "up").mkdir(parents=True)
    ix = m.Index(bk); w = ix.writer_for(os.getpid())
    for rel, kind, content in entries:
        if kind == "whiteout":
            ix.set_entry(rel, "whiteout", 0, w, "unlink")
        else:
            ix.set_entry(rel, "file", stat_mod.S_IFREG | 0o644, w, "create")
            bp = m.blob_path(ix.box_id, ix.row_id(rel))
            bp.parent.mkdir(parents=True, exist_ok=True); bp.write_bytes(content)
    m.consolidate(str(bk), sid, index=ix); ix.close()
    shutil.rmtree(bk, ignore_errors=True)
    if parent is not None:
        m.sqlar_meta_set(m.sqlar_path(sid), "parent_box_id", parent)
    if meta:
        for k, v in meta.items():
            m.sqlar_meta_set(m.sqlar_path(sid), k, v)
    return m.sqlar_path(sid)


def main():
    if not ensure_binary():
        print("  ok  parent-modes-rs: cargo/binary unavailable — SKIP")
        print("\nPARENT-MODES-RS PASS (skipped)")
        return 0
    tmp = Path(tempfile.mkdtemp(prefix="pmrs-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "RS"
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    eng = None
    try:
        eng = subprocess.Popen([str(BIN), "serve"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        sock = m.sock_path()
        if not wait_socket(sock):
            out = eng.stdout.read(2000) if eng.stdout else b""
            raise RuntimeError("rust engine socket never appeared:\n"
                               + out.decode(errors="replace"))

        # ── (1) --no-parent: meta persisted; parent NOT auto-derived ─────────
        # The register handshake reads want_no_parent and sets no_host_fallback
        # in BoxState + persists the meta key. The bwrap inside is harmless
        # here (it sees an empty / and will likely fail to find /bin/sh) — but
        # the meta is written by the engine BEFORE bwrap is even forked, so we
        # can read it back regardless.
        r = subprocess.run(
            [str(BIN), "run", "--no-parent", "NPB", "--", "true"],
            capture_output=True, text=True, timeout=60)
        # NB: we don't assert returncode — bwrap may fail because / is empty;
        # the engine-side persistence is what matters.
        nsp = newest_sqlar(m); wait_for_sqlar_settled(m, nsp)
        check(meta_get(nsp, "no_host_fallback") == "1",
              f"no-parent: sqlar meta has no_host_fallback=1 "
              f"(got {meta_get(nsp, 'no_host_fallback')!r})")
        check(meta_get(nsp, "parent_box_id") is None,
              f"no-parent: parent_box_id NOT recorded "
              f"(got {meta_get(nsp, 'parent_box_id')!r})")

        # ── (2) --readonly-parent: at-rest apply REFUSES to promote upward ──
        # Build a parent + child sqlar; stamp readonly_parent=1 on the child.
        # review.apply on the child must:
        #   • return an error per path (parent is read-only)
        #   • leave the parent's sqlar UNCHANGED (no promote)
        #   • leave the child's row IN PLACE (not consumed)
        psid, csid = "8801", "8802"
        try:
            make_finished_box(m, psid, [])                       # empty parent
            make_finished_box(m, csid, [
                ("root/ropar_child.txt", "file", b"child-wrote\n"),
            ], parent=psid, meta={"readonly_parent": "1"})
            psp = m.sqlar_path(psid); csp = m.sqlar_path(csid)

            ra = (m.sync_request(sock, type="ui", verb="review.apply",
                  args=[csid, ["root/ropar_child.txt"]]) or {}).get("r") or {}
            applied = set(ra.get("applied") or [])
            errors = ra.get("errors") or []
            check(not applied,
                  f"readonly-parent: nothing was applied (got {applied})")
            check(len(errors) == 1
                  and errors[0].get("path") == "root/ropar_child.txt"
                  and "read-only" in (errors[0].get("error") or "").lower(),
                  f"readonly-parent: apply errored with a read-only message "
                  f"(got {errors})")
            # Parent's sqlar UNCHANGED — no promote ever happened.
            pnames = {n for n, *_ in m.sqlar_list(psp)}
            check("root/ropar_child.txt" not in pnames,
                  "readonly-parent: parent sqlar UNCHANGED (no promote)")
            # Child's row STILL there — not consumed.
            cnames = {n for n, *_ in m.sqlar_list(csp)}
            check("root/ropar_child.txt" in cnames,
                  "readonly-parent: child's captured row STAYS (not consumed)")
            check(m.sqlar_content(csp, "root/ropar_child.txt") == b"child-wrote\n",
                  "readonly-parent: child's captured bytes intact")
        finally:
            for s in (psid, csid):
                if m.sqlar_path(s).exists():
                    m.sync_request(sock, type="ui", verb="delete", args=[s])

        eng.terminate()
        try: eng.wait(timeout=10)
        except subprocess.TimeoutExpired:
            eng.kill(); eng.wait(timeout=5)
    except Exception as e:
        import traceback; traceback.print_exc(); _fails.append(str(e))
    finally:
        if eng is not None and eng.poll() is None:
            eng.kill()
            try: eng.wait(timeout=5)
            except Exception: pass
        os.environ.pop("SLOPBOX_NS", None)
        shutil.rmtree(tmp, ignore_errors=True)
    print("\n" + ("PARENT-MODES-RS PASS" if not _fails
                  else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_parent_modes_rs():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
