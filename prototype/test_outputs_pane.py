#!/usr/bin/env -S uv run --with pytest --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["textual>=0.60","wcmatch>=8.4","pyfuse3>=3.2",
#                 "trio>=0.22","python-magic>=0.4"]
# ///
"""Pilot test for the OUTPUTS pane (the `o` view).

Drives the REAL Textual app under app.run_test() against a finished box whose sqlar
has a process table (≥2 processes) and a handful of captured stdout/stderr writes,
and asserts the pane's distinctive behaviour:

  • the left #out-tab lists every captured write (Time · Stream · Process · Bytes);
  • the right #out-detail CONCATENATES the content of ALL listed entries in id order,
    with the SELECTED entry's lines marked in the left margin (▌ gutter);
  • a '/' process filter (exe:) narrows BOTH the list AND the concatenated content;
  • the procs↔outputs generated "ids" navigation pins each pane to the other's subset.

Runnable standalone (./test_outputs_pane.py) and under pytest.
"""
import asyncio
import os
import shutil
import sys
import tempfile
from pathlib import Path
from importlib.machinery import SourceFileLoader

SARUN = str(Path(__file__).resolve().parent / "sarun")
m = SourceFileLoader("slopbox", SARUN).load_module()

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


def _seed_box(sid, writes, procs):
    """Build a FINISHED box `sid` with a process table and captured-write rows.

    `procs` = [(tgid, exe), ...]  → process rows (the first is the hierarchy root).
    `writes` = [(proc_index, stream, content_bytes), ...] → outputs rows in order.
    Returns (Supervisor, {proc_index -> process_row_id}).
    The Index db IS the box's sqlar, so add_output writes straight to it.
    """
    backing = m.live_dir(sid); (backing / "up").mkdir(parents=True, exist_ok=True)
    idx = m.Index(backing)
    row_ids = {}
    for i, (tgid, exe) in enumerate(procs):
        rid = idx.process_from_prov(
            dict(tgid=tgid, start=1000 + i, ppid=(procs[0][0] if i else 0),
                 exe=exe, cwd="/work", argv=[exe, f"arg{i}"]),
            root=(i == 0))
        row_ids[i] = rid
    for pidx, stream, content in writes:
        idx.add_output(row_ids[pidx], stream, content)
    idx.close()

    sup = m.Supervisor(mount=None)
    sup.sessions[sid] = m.Session(
        session_id=sid, box_id=int(sid), cmd=["sh"],
        live=False, shm_dir=str(backing))
    sup.review._consolidated.add(sid)   # already folded: skip the consolidate path
    return sup, row_ids


async def drive(m):
    from textual.widgets import DataTable, Static

    sid = "8201"
    # Two processes; writes interleave stdout/stderr across both, in id order.
    procs = [(5001, "/usr/bin/python3"), (5002, "/bin/sh")]
    writes = [
        (0, 0, b"py-out-1\n"),         # python3 stdout
        (1, 1, b"sh-err-1\n"),         # sh     stderr
        (0, 0, b"py-out-2\n"),         # python3 stdout
        (1, 0, b"sh-out-1\n"),         # sh     stdout
    ]
    sup, rids = _seed_box(sid, writes, procs)

    UI = m._make_ui_app()
    app = UI()
    # Swap in our pre-seeded Supervisor + add the columns on_mount normally would.
    async def _noop_mount(self):
        self.query_one("#s-tab").add_columns("F","Name","PID","Cmd","Age")
        self.query_one("#cf-tab").add_columns("","Path","Size")
        self.query_one("#pr-tab").add_columns("TGID","PPID","Exe","Argv")
        self.query_one("#out-tab").add_columns("Time","Stream","Process","Bytes")
        self.sup = sup
        self._sel_sid = sid
        self._set_view("boxes")
    UI.on_mount = _noop_mount

    async with app.run_test(size=(120, 40)) as pilot:
        await pilot.pause()

        # ── open the outputs pane via its key ───────────────────────────────
        await pilot.press("o"); await pilot.pause()
        check(app.view == "outputs", "'o' selects the outputs view")
        check(app.RIGHT["outputs"] == "#out-wrap" and app.LEFT["outputs"] == "#out-tab",
              "outputs wired into LEFT/RIGHT maps")

        out = app.query_one("#out-tab", DataTable)
        check(out.row_count == 4, f"list shows all 4 captured writes (got {out.row_count})")
        # row keys are the output ids, in id order
        keys = [r.value for r in out.rows]
        check(keys == sorted(keys) and len(keys) == 4, "rows keyed by output id, id order")

        # The "Process" column resolves exe basename + pid for each entry.
        cells = out.get_row_at(0)
        check(any("python3" in str(c) for c in cells) and any("5001" in str(c) for c in cells),
              "Process column shows exe basename + pid (python3·5001)")
        check(str(cells[1]) in ("out", "err"), "Stream column shows out/err")

        # ── concatenated right pane: ALL listed content, selected entry marked ──
        detail = app.query_one("#out-detail", Static)
        # select the first row (py-out-1)
        out.move_cursor(row=0, animate=False); await pilot.pause()
        txt = detail.render().plain
        for piece in (b"py-out-1", b"sh-err-1", b"py-out-2", b"sh-out-1"):
            check(piece.decode() in txt,
                  f"right pane concatenates listed content ({piece.decode()!r})")
        # the selected entry's line carries the ▌ gutter marker; only its lines do.
        sel_lines = [ln for ln in txt.splitlines() if "py-out-1" in ln]
        other_lines = [ln for ln in txt.splitlines() if "sh-err-1" in ln]
        check(sel_lines and "▌" in sel_lines[0],
              "selected entry's line is marked with the ▌ gutter")
        check(other_lines and "▌" not in other_lines[0],
              "non-selected entries' lines are NOT marked")

        # moving the cursor moves the marker
        out.move_cursor(row=1, animate=False); await pilot.pause()
        txt2 = detail.render().plain
        moved_sel = [ln for ln in txt2.splitlines() if "sh-err-1" in ln]
        moved_other = [ln for ln in txt2.splitlines() if "py-out-1" in ln]
        check(moved_sel and "▌" in moved_sel[0], "marker follows the cursor to the new selection")
        check(moved_other and "▌" not in moved_other[0], "previously-selected line is no longer marked")

        # ── '/' process filter narrows BOTH the list and the concatenation ──
        # Filter to exe:**/python3 — only the two python3 writes survive.
        app._view_filters["outputs"] = {
            "clauses": [m.Clause(m.Match("exe", "**/python3"))],
            "on": True, "generated": False}
        app._reload_view("outputs"); await pilot.pause()
        out = app.query_one("#out-tab", DataTable)
        check(out.row_count == 2, f"exe filter narrows list to the 2 python3 writes (got {out.row_count})")
        ftxt = app.query_one("#out-detail", Static).render().plain
        check("py-out-1" in ftxt and "py-out-2" in ftxt, "filtered concatenation keeps python3 output")
        check("sh-err-1" not in ftxt and "sh-out-1" not in ftxt,
              "filtered concatenation DROPS the sh writes")
        # clear the filter
        app._view_filters["outputs"] = {"clauses": [], "on": False, "generated": False}
        app._reload_view("outputs"); await pilot.pause()
        check(app.query_one("#out-tab", DataTable).row_count == 4, "clearing the filter restores all 4")

        # ── procs → outputs: pins outputs to the selected process's writes ──
        await pilot.press("p"); await pilot.pause()
        check(app.view == "procs", "'p' switches to the procs pane")
        pr = app.query_one("#pr-tab", DataTable)
        # park on the sh process row (row id rids[1]) and cross-navigate with 'o'
        pr.move_cursor(row=pr.get_row_index(str(rids[1])), animate=False); await pilot.pause()
        check(app._sel_proc() == rids[1], "cursor parked on the sh process row")
        await pilot.press("o"); await pilot.pause()
        check(app.view == "outputs", "'o' from procs lands on the outputs pane")
        check(app._view_filters["outputs"]["generated"] is True,
              "procs→outputs built a generated 'ids' filter")
        out = app.query_one("#out-tab", DataTable)
        # sh wrote two of the four writes (one stderr, one stdout)
        check(out.row_count == 2, f"outputs pinned to sh's 2 writes (got {out.row_count})")
        otxt = app.query_one("#out-detail", Static).render().plain
        check("sh-err-1" in otxt and "sh-out-1" in otxt and "py-out-1" not in otxt,
              "pinned concatenation shows only sh's output")

        # ── outputs → procs: pins procs to the selected entry's process ──
        out.move_cursor(row=0, animate=False); await pilot.pause()
        sel_oid = app._sel_output()
        check(app._output_pid(sel_oid) == rids[1], "selected entry resolves to the sh process_id")
        await pilot.press("p"); await pilot.pause()
        check(app.view == "procs", "'p' from outputs lands on the procs pane")
        check(app._view_filters["procs"]["generated"] is True,
              "outputs→procs built a generated 'ids' filter")
        pr = app.query_one("#pr-tab", DataTable)
        check(pr.row_count == 1 and app._sel_proc() == rids[1],
              "procs pinned to exactly the sh process")

        # esc clears the generated filter (back to the full proc list)
        await pilot.press("escape"); await pilot.pause()
        check(app._view_filters["procs"]["on"] is False, "esc cancels the generated procs filter")


def test_outputs_pane():
    tmp = Path(tempfile.mkdtemp(prefix="out-pane-"))
    os.environ["XDG_STATE_HOME"]  = str(tmp / "state")
    os.environ["XDG_RUNTIME_DIR"] = str(tmp / "run")
    os.environ["XDG_CONFIG_HOME"] = str(tmp / "config")
    os.environ["XDG_DATA_HOME"]   = str(tmp / "data")
    for p in ("state", "run", "config", "data"):
        (tmp / p).mkdir(parents=True, exist_ok=True)
    m.ensure_dirs()
    try:
        asyncio.run(drive(m))
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    if _fails:
        raise AssertionError(f"{len(_fails)} check(s) failed: {_fails}")


if __name__ == "__main__":
    try:
        test_outputs_pane()
    except AssertionError as e:
        print(f"\n{e}", file=sys.stderr); sys.exit(1)
    print("\nOUTPUTS-PANE PASS"); sys.exit(0)
