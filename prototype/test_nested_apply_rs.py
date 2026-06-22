#!/usr/bin/env python3
"""Nested-box apply/discard semantics against the RUST engine (engine/).

The audit found nested apply/discard ABSENT in the Rust port: a box with a
parent must PROMOTE an applied change INTO THE PARENT's overlay (a pending
change in the parent box) instead of writing the real host — only a TOP-LEVEL
box's apply reaches the host. A discard must COPY the path DOWN into immediate
children that inherit it before dropping the row, so a descendant's merged view
is unchanged. This proves all three with REAL effects (not shape):

  • nested APPLY: a child box's captured change, applied → appears as a PENDING
    change in the PARENT box's sqlar; the real HOST is UNTOUCHED. Applying the
    PARENT (now top-level) then reaches the host.
  • discard COPY-DOWN: a parent path a child inherited (never touched) →
    discarding it in the parent copies it DOWN so the child still resolves it;
    the host is untouched. Also proven via finalize_by_rules (dissolve path).
  • top-level apply still WRITES the host (no regression).

Boxes are built at-rest (sqlar only) via the Python module helpers, exactly as
test_engine_rs.py's dissolve/copy-down cases do — read back with the Python
sqlar readers (rusqlite wrote them; the formats match).

Run:
    uv run --with "pyfuse3>=3.2" --with "trio>=0.22" --with "wcmatch>=8.4" \
      --with "python-magic>=0.4" python test_nested_apply_rs.py
Skips (passes vacuously) if cargo/the binary are unavailable.
"""
import os, socket, stat as stat_mod, subprocess, sys, tempfile, shutil, time
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


def make_finished_box(m, sid, entries, parent=None):
    """Build an at-rest (sqlar-only) box `sid` capturing `entries`:
    a list of (rel, kind, content) where kind is 'file' or 'whiteout'.
    Optionally records `parent` as parent_box_id. Returns the sqlar path."""
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
    return m.sqlar_path(sid)


def main():
    if not ensure_binary():
        raise SystemExit("test_nested_apply_rs: engine binary unavailable — run `make engine`")
    tmp = Path(tempfile.mkdtemp(prefix="nestrs-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    os.environ["SLOPBOX_NS"] = "NEST"
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

        # ════════════════════════════════════════════════════════════════════
        #  (1) NESTED APPLY: child applied → PROMOTE into the PARENT overlay,
        #      host UNTOUCHED. Then apply the parent → reaches the host.
        # ════════════════════════════════════════════════════════════════════
        # Parent box (top-level) is EMPTY; child box captures a created file and
        # a deletion (whiteout). The deletion's host victim DOES exist, so the
        # promote must carry it into the parent as a whiteout (parent's lower —
        # the host — still resolves the path).
        host_new = Path("/root/nest_apply_new.txt")
        host_del = Path("/root/nest_apply_del.txt")
        host_new.unlink(missing_ok=True)
        host_del.write_bytes(b"victim-on-host\n")
        psid, csid = "8001", "8002"
        try:
            make_finished_box(m, psid, [])                 # empty parent
            make_finished_box(m, csid, [
                ("root/nest_apply_new.txt", "file", b"child-made\n"),
                ("root/nest_apply_del.txt", "whiteout", None),
            ], parent=psid)

            psp = m.sqlar_path(psid); csp = m.sqlar_path(csid)
            # Sanity: before apply, the parent has NEITHER path.
            pnames0 = {n for n, *_ in m.sqlar_list(psp)}
            check("root/nest_apply_new.txt" not in pnames0
                  and "root/nest_apply_del.txt" not in pnames0,
                  "nested-apply: parent has no captured paths before the apply")

            ra = (m.sync_request(sock, type="ui", verb="review.apply",
                  args=[csid, ["root/nest_apply_new.txt",
                               "root/nest_apply_del.txt"]]) or {}).get("r") or {}
            applied = set(ra.get("applied") or [])
            check({"root/nest_apply_new.txt", "root/nest_apply_del.txt"} <= applied,
                  "nested-apply: review.apply reports both child paths applied")
            check(not (ra.get("errors") or []),
                  f"nested-apply: apply had no errors (got {ra.get('errors')})")

            # The REAL host is UNTOUCHED — a nested apply never reaches it.
            check(not host_new.exists(),
                  "nested-apply: created file was NOT written to the real host")
            check(host_del.exists() and host_del.read_bytes() == b"victim-on-host\n",
                  "nested-apply: host deletion victim still present (host untouched)")

            # The change now lives as a PENDING change in the PARENT's overlay.
            pnames = {n: mode for n, mode, *_ in m.sqlar_list(psp)}
            check(m.sqlar_content(psp, "root/nest_apply_new.txt") == b"child-made\n",
                  "nested-apply: created file PROMOTED into the PARENT overlay "
                  "(child bytes, python-readable from the parent sqlar)")
            check(stat_mod.S_ISCHR(pnames.get("root/nest_apply_del.txt", 0)),
                  "nested-apply: deletion PROMOTED into the PARENT as a whiteout "
                  "(parent's lower — the host — still has the file)")

            # The child's rows were consumed (it applied them away).
            cnames = {n for n, *_ in m.sqlar_list(csp)} if csp.exists() else set()
            check("root/nest_apply_new.txt" not in cnames
                  and "root/nest_apply_del.txt" not in cnames,
                  "nested-apply: child's applied rows consumed from its archive")

            # Now APPLY the PARENT (top-level) → the promoted changes reach host.
            rap = (m.sync_request(sock, type="ui", verb="review.apply",
                   args=[psid, ["root/nest_apply_new.txt",
                                "root/nest_apply_del.txt"]]) or {}).get("r") or {}
            check(not (rap.get("errors") or []),
                  f"nested-apply: parent apply had no errors (got {rap.get('errors')})")
            check(host_new.exists() and host_new.read_bytes() == b"child-made\n",
                  "nested-apply: applying the PARENT WROTE the file to the host")
            check(not host_del.exists(),
                  "nested-apply: applying the PARENT removed the tombstoned host file")
        finally:
            host_new.unlink(missing_ok=True); host_del.unlink(missing_ok=True)
            for s in (psid, csid):
                if m.sqlar_path(s).exists():
                    m.sync_request(sock, type="ui", verb="delete", args=[s])

        # ════════════════════════════════════════════════════════════════════
        #  (2) DISCARD COPY-DOWN: a parent path a child inherited → discarding
        #      it in the parent copies it DOWN so the child still resolves it.
        #      Host untouched.
        # ════════════════════════════════════════════════════════════════════
        host_inh = Path("/root/nest_disc_inh.txt"); host_inh.unlink(missing_ok=True)
        ppid, ccid = "8101", "8102"
        try:
            # Parent captures a created file; child has NO entry of its own (it
            # only inherits the parent's view). Parent is TOP-LEVEL so its lower
            # is the host — the host does NOT have the file, so the parent's row
            # is the only thing the child sees for it.
            make_finished_box(m, ppid, [
                ("root/nest_disc_inh.txt", "file", b"inherited-bytes\n")])
            make_finished_box(m, ccid, [], parent=ppid)
            psp = m.sqlar_path(ppid); csp = m.sqlar_path(ccid)

            cnames0 = {n for n, *_ in m.sqlar_list(csp)} if csp.exists() else set()
            check("root/nest_disc_inh.txt" not in cnames0,
                  "discard-copydown: child has NO own entry before the discard")

            rd = (m.sync_request(sock, type="ui", verb="review.discard",
                  args=[ppid, ["root/nest_disc_inh.txt"]]) or {}).get("r") or {}
            check("root/nest_disc_inh.txt" in (rd.get("discarded") or []),
                  "discard-copydown: review.discard reports the path discarded")
            check(not (rd.get("errors") or []),
                  f"discard-copydown: discard had no errors (got {rd.get('errors')})")

            # Host untouched (discard never writes the host).
            check(not host_inh.exists(),
                  "discard-copydown: discard did NOT write the file to the host")
            # The file was copied DOWN into the child so its view is unchanged.
            check(m.sqlar_content(csp, "root/nest_disc_inh.txt") == b"inherited-bytes\n",
                  "discard-copydown: parent path COPIED DOWN into the child "
                  "(child still resolves the inherited bytes)")
        finally:
            host_inh.unlink(missing_ok=True)
            for s in (ppid, ccid):
                if m.sqlar_path(s).exists():
                    m.sync_request(sock, type="ui", verb="delete", args=[s])

        # ── 2b: discard copy-down ALSO via finalize_by_rules (dissolve) ──────
        # A discard RULE keeps the inherited file off the host on dissolve; the
        # ONLY way the child keeps seeing it is the finalize discard pass copying
        # it down. (test_engine_rs covers this for a copy-down-then-free path;
        # here we additionally assert the finalize DISCARD set itself copies
        # down — the gap the audit named: finalize split-but-did-not-copy-down.)
        rules_f = Path(os.environ["XDG_CONFIG_HOME"]) / "slopbox.NEST" / "filerules"
        rules_f.parent.mkdir(parents=True, exist_ok=True)
        rules_f.write_text("discard **/*.inh\n")
        m.sync_request(sock, type="ui", verb="reload_rules", args=[])
        fpid, fcid = "8201", "8202"
        host_finh = Path("/root/fin_shared.inh"); host_finh.unlink(missing_ok=True)
        try:
            make_finished_box(m, fpid, [
                ("root/fin_shared.inh", "file", b"finalized-inherit\n")])
            make_finished_box(m, fcid, [], parent=fpid)
            dr = (m.sync_request(sock, type="ui", verb="dissolve", args=[fpid])
                  or {}).get("r") or {}
            check(dr.get("ok") is True,
                  "finalize-copydown: dissolve (finalize_by_rules) of a box with a "
                  "child succeeds")
            check(not host_finh.exists(),
                  "finalize-copydown: discard-ruled inherited file NOT applied to host")
            check(m.sqlar_content(m.sqlar_path(fcid), "root/fin_shared.inh")
                  == b"finalized-inherit\n",
                  "finalize-copydown: finalize's DISCARD pass COPIED the path DOWN "
                  "into the child (view preserved)")
            check(not m.sqlar_path(fpid).exists(),
                  "finalize-copydown: dissolved parent freed")
        finally:
            host_finh.unlink(missing_ok=True)
            rules_f.unlink(missing_ok=True)
            m.sync_request(sock, type="ui", verb="reload_rules", args=[])
            if m.sqlar_path(fcid).exists():
                m.sync_request(sock, type="ui", verb="delete", args=[fcid])

        # ════════════════════════════════════════════════════════════════════
        #  (3) TOP-LEVEL apply still WRITES the host (no regression).
        # ════════════════════════════════════════════════════════════════════
        host_top = Path("/root/nest_top_apply.txt"); host_top.unlink(missing_ok=True)
        tsid = "8301"
        try:
            make_finished_box(m, tsid, [
                ("root/nest_top_apply.txt", "file", b"top-level!\n")])  # no parent
            rt = (m.sync_request(sock, type="ui", verb="review.apply",
                  args=[tsid, ["root/nest_top_apply.txt"]]) or {}).get("r") or {}
            check("root/nest_top_apply.txt" in (rt.get("applied") or []),
                  "top-level-apply: review.apply reports the path applied")
            check(host_top.exists() and host_top.read_bytes() == b"top-level!\n",
                  "top-level-apply: a TOP-LEVEL box's apply WROTE the host (regression)")
        finally:
            host_top.unlink(missing_ok=True)
            if m.sqlar_path(tsid).exists():
                m.sync_request(sock, type="ui", verb="delete", args=[tsid])

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
    print("\n" + ("NESTED-APPLY-RS PASS" if not _fails
                  else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_nested_apply_rs():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
