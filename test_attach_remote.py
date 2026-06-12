#!/usr/bin/env python3
"""Attach-mode parity: the SAME Textual UI runs as a pure CLIENT of a real engine
subprocess — every pane reads over the control socket; no mount, no server in the
UI process. Drives the remote app with app.run_test() against an engine that
discovered real boxes on disk, and checks end-to-end through the RPC layer:
  · sessions/changes/procs/outputs panes populate over the wire
  · the diff view renders hunks; hunk APPLY writes the real host file and hunk
    DISCARD drops the change — both through review verbs
  · a not-yet-consolidated box folds ENGINE-side (consolidate_start verb +
    done event) and its changes then render
  · a binary (ELF) change renders the structural-diff quick half and the heavy
    engine-side sandboxed parser completes (job registry + struct_finish)
Run:
    uv run --with "textual>=0.60" --with "pyfuse3>=3.2" --with "trio>=0.22" \
      --with "wcmatch>=8.4" --with "python-magic>=0.4" python test_attach_remote.py
Self-safety: XDG dirs redirected to a temp tree; the engine subprocess is killed
in finally; hunk-apply targets /root/attach_e2e_apply.txt (removed in finally).
"""
import os, sys, asyncio, socket, subprocess, tempfile, shutil, time
import stat as stat_mod
from pathlib import Path
from importlib.machinery import SourceFileLoader

SARUN = "/home/user/sarun/sarun"
APPLY_HOST = Path("/root/attach_e2e_apply.txt")

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


def wait_socket(sock, timeout=60):
    end = time.time() + timeout
    while time.time() < end:
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
                s.settimeout(1.0); s.connect(sock); return True
        except OSError:
            time.sleep(0.2)
    return False


def _mk_box(m, sid, files, consolidate=True):
    """A box on disk: {rel: bytes} captured entries (+ one process row)."""
    backing = m.live_dir(sid); (backing / "up").mkdir(parents=True)
    idx = m.Index(backing)
    wid = idx.writer_for(os.getpid())
    for rel, content in files.items():
        idx.set_entry(rel, "file", stat_mod.S_IFREG | 0o644, wid, "create")
        bp = m.blob_path(idx.box_id, idx.row_id(rel))
        bp.parent.mkdir(parents=True, exist_ok=True)
        bp.write_bytes(content)
    if consolidate:
        m.consolidate(str(backing), sid, index=idx)
    idx.close()
    return wid


def _dv_text(app) -> str:
    """Flatten the DiffView's rendered content to one searchable string."""
    out = []
    dv = app.query_one("#cd-log")
    for it in dv.children:
        if hasattr(it, "_lines"):
            out += [f"{t}{x}" for t, x in it._lines]
        else:
            for lab in it.query("Label"):
                try: out.append(str(lab.render()))
                except Exception: pass
    return "\n".join(out)


async def _focus_change(app, pilot, rel):
    tbl = app.query_one("#cf-tab")
    tbl.move_cursor(row=tbl.get_row_index(rel), animate=False)
    await pilot.pause(0.2); await pilot.pause()
    # move_cursor onto the SAME row fires no RowHighlighted event — load directly
    # (idempotent; the same path the highlight handler's debounce takes).
    app._load_diff(app._sel_sid, rel)
    await pilot.pause(0.2); await pilot.pause()


async def drive(m, sid):
    app = m._make_ui_app()(remote=True)
    async with app.run_test(size=(120, 40)) as pilot:
        await pilot.pause()
        check(app.remote and app.overlay_mount is None,
              "attach: remote app holds no local mount")
        check(type(app.sup).__name__ == "RemoteSupervisor",
              "attach: sup is the RPC facade")
        check(sid in app.sessions, "attach: finished box listed over the wire")
        app._select_sid(sid); await pilot.pause()

        # ── changes pane + diff view ─────────────────────────────────────────
        await pilot.press("c"); await pilot.pause(0.3); await pilot.pause()
        rels = {e.get("path") for e in app._ch_entries}
        check({"root/attach_e2e_apply.txt", "root/attach_e2e_discard.txt",
               "root/attach_e2e_elf"} <= rels,
              "attach: changes pane shows all captured files")

        await _focus_change(app, pilot, "root/attach_e2e_apply.txt")
        txt = _dv_text(app)
        check("+hello hunk" in txt,
              "attach: diff view renders the created file's hunk over the wire")

        # hunk APPLY: writes the REAL host file via the engine
        app._hunk_apply(0); await pilot.pause(0.3); await pilot.pause()
        check(APPLY_HOST.exists() and APPLY_HOST.read_bytes() == b"hello hunk\n",
              "attach: hunk apply wrote the host file through the engine")

        # hunk DISCARD: the change disappears from the set
        await _focus_change(app, pilot, "root/attach_e2e_discard.txt")
        app._hunk_discard(0); await pilot.pause(0.3); await pilot.pause()
        check(all(e["path"] != "root/attach_e2e_discard.txt"
                  for e in app._ch_entries),
              "attach: hunk discard dropped the change over the wire")

        # ── structural diff (binary/ELF) — engine-side sandboxed parser ────
        await _focus_change(app, pilot, "root/attach_e2e_elf")
        txt = _dv_text(app)
        check("ELF" in txt, "attach: struct quick half shows the libmagic type")
        if "structural diff" in txt:        # recognized type → engine-side job
            end = time.time() + 30
            while time.time() < end and app._struct_spin_item is not None:
                await pilot.pause(0.3)
            check(app._struct_spin_item is None,
                  "attach: engine-side structural parser completed (job finished)")

        # ── procs / outputs panes ────────────────────────────────────────────
        await pilot.press("p"); await pilot.pause(0.3); await pilot.pause()
        check(len(app._pr_procs) >= 1, "attach: procs pane has the writer row")
        await pilot.press("o"); await pilot.pause()
        check(app.view == "outputs", "attach: outputs view renders")
        await pilot.press("b"); await pilot.pause()

        # ── engine-side consolidation of a NOT-yet-folded box ───────────────
        sid2 = "4002"
        _mk_box(m, sid2, {"root/attach_e2e_fold.txt": b"folded\n"},
                consolidate=False)
        app.sup.rescan(); await pilot.pause(0.3); await pilot.pause()
        check(sid2 in app.sup.sessions,
              "attach: rescan discovers the new box over the wire")
        app.sessions = {**app.sessions, **app.sup.sessions}
        app._rebuild_sessions(); app._select_sid(sid2)
        await pilot.press("c"); await pilot.pause(0.3)
        end = time.time() + 30
        while time.time() < end:
            if any(e.get("path") == "root/attach_e2e_fold.txt"
                   for e in app._ch_entries):
                break
            await pilot.pause(0.5)
            app._load_changes(sid2)
        check(any(e.get("path") == "root/attach_e2e_fold.txt"
                  for e in app._ch_entries),
              "attach: engine-side consolidate folded the box; changes render")
        check(sid2 not in app.sup.review._consolidating,
              "attach: review_state shows the fold finished")
    # quitting an ATTACHED (not auto-spawned) UI must leave the engine running
    check(wait_socket(os.environ["_ENG_SOCK"], 5),
          "attach: engine still answers after the attached UI quit")


def main():
    tmp = Path(tempfile.mkdtemp(prefix="attach-"))
    for k, sub in (("XDG_STATE_HOME", "state"), ("XDG_RUNTIME_DIR", "run"),
                   ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")):
        os.environ[k] = str(tmp / sub)
    (tmp / "run").mkdir(parents=True, exist_ok=True)
    m = SourceFileLoader("slopbox", SARUN).load_module()
    m.ensure_dirs()
    eng = None
    try:
        sid = "4001"
        _mk_box(m, sid, {
            "root/attach_e2e_apply.txt": b"hello hunk\n",
            "root/attach_e2e_discard.txt": b"discard me\n",
            "root/attach_e2e_elf": Path("/bin/true").read_bytes(),
        })
        eng = subprocess.Popen([sys.executable, SARUN, "engine"],
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        os.environ["_ENG_SOCK"] = m.sock_path()
        if not wait_socket(m.sock_path(), 60):
            out = eng.stdout.read(4000) if eng.stdout else b""
            raise RuntimeError("engine socket never appeared:\n"
                               + out.decode(errors="replace"))
        asyncio.run(drive(m, sid))
    except Exception as e:
        import traceback; traceback.print_exc(); _fails.append(str(e))
    finally:
        if eng is not None:
            eng.terminate()
            try: eng.wait(timeout=15)
            except Exception: eng.kill()
        APPLY_HOST.unlink(missing_ok=True)
        shutil.rmtree(tmp, ignore_errors=True)
    print("\n" + ("ATTACH PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


def test_attach_remote():
    assert main() == 0, _fails


if __name__ == "__main__":
    sys.exit(main())
