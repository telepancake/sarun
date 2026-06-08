#!/usr/bin/env python3
"""End-to-end test of the Changes view's incremental rendering.

Drives the REAL app's _rebuild_ch() path (the one a live build hammers through
_load_changes) against the real #cf-tab DataTable, and asserts that a churning
change set — files appearing all over the tree, some vanishing — updates the table
WITHOUT recreating rows and WITHOUT moving the user's cursor. This is the regression
guard for the "freezes + cursor thrown across the lawn at 40k files" report.

Run:
    /home/user/venv/bin/python test_changes_view_incremental.py
"""
import os, sys, asyncio, tempfile, shutil, subprocess
from pathlib import Path
from importlib.machinery import SourceFileLoader

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


def entries_for(paths):
    return [dict(path=p, kind="created", size=0) for p in paths]


async def drive(m):
    from textual.widgets import DataTable
    from textual.widgets._data_table import RowKey

    app = m._make_ui_app()()
    async with app.run_test(size=(60, 24)) as pilot:
        await pilot.pause()
        await pilot.press("c"); await pilot.pause()          # changes view
        check(app.view == "changes", "changes view active")
        cf = app.query_one("#cf-tab", DataTable)
        app._sel_sid = None                                   # disable lazy decoration

        # ── initial set: 300 files spread across a directory tree ───────────
        base = [f"src/mod{ i//25 :02d}/file{i:04d}.c" for i in range(300)]
        app._ch_entries = entries_for(base)
        app._ch_index = {e["path"]: e for e in app._ch_entries}
        app._rebuild_ch()
        await pilot.pause()
        n_file_rows = sum(1 for *_x, conn in app._ch_rows if not conn)
        check(n_file_rows == 300, f"initial: 300 file rows in tree (got {n_file_rows})")
        check(cf.row_count == len(app._ch_rows),
              "initial: table row_count matches tree (files + connector dirs)")

        # park the cursor on a deep real file mid-list and record its screen position
        target = "src/mod06/file0160.c"
        cf.move_cursor(row=cf.get_row_index(target), animate=False)
        await pilot.pause()
        cur0 = cf.coordinate_to_cell_key((cf.cursor_row, 0)).row_key.value
        y0 = cf.cursor_row - int(cf.scroll_offset.y)
        check(cur0 == target, f"cursor parked on {target}")

        # snapshot survivor Row identities
        ident0 = {rk.value: id(cf.rows[rk]) for rk in cf.rows}

        # ── churn like a build: 60 new files scattered (many sort ABOVE the ──
        # cursor), 10 removed — exactly the pattern that used to clear+rebuild. ─
        new = ([f"src/mod{ k :02d}/zextra{ j }.o" for k in range(7) for j in range(6)]
               + [f"src/aaa/early{ j :02d}.h" for j in range(18)])
        gone = set(base[5:15])
        churned = [p for p in base if p not in gone] + new
        app._ch_entries = entries_for(sorted(set(churned)))
        app._ch_index = {e["path"]: e for e in app._ch_entries}
        app._rebuild_ch()
        await pilot.pause()

        keys_now = {rk.value for rk in cf.rows}
        check(all(p in keys_now for p in new), "churn: all new files present in table")
        check(not (gone & keys_now), "churn: removed files gone from table")

        # display order is exactly the DFS tree order (no stale/duplicate rows)
        want = [rk for rk, *_r in app._ch_rows]
        got = [r.key.value for r in cf.ordered_rows]
        check(got == want, "churn: table display order == DFS tree order")

        # NO REBUILD: every survivor kept its Row object identity
        survivors = [p for p in base if p not in gone]
        same = all(p in keys_now and id(cf.rows[RowKey(p)]) == ident0[p]
                   for p in survivors)
        check(same, "churn: survivors kept their Row identity (no clear+re-add)")

        # CURSOR: still on the same file, still at the same screen line, despite
        # ~18 new rows inserted above it (src/aaa/* sorts before src/mod06/*).
        cur1 = cf.coordinate_to_cell_key((cf.cursor_row, 0)).row_key.value
        y1 = cf.cursor_row - int(cf.scroll_offset.y)
        check(cur1 == target, f"churn: cursor still on {target} (got {cur1})")
        check(y1 == y0, f"churn: cursor held its screen line (was {y0}, now {y1})")


def main():
    tmp = Path(tempfile.mkdtemp(prefix="ch-incr-"))
    os.environ["XDG_STATE_HOME"] = str(tmp/"state")
    os.environ["XDG_RUNTIME_DIR"] = str(tmp/"run")
    os.environ["XDG_CONFIG_HOME"] = str(tmp/"config")
    os.environ["XDG_DATA_HOME"] = str(tmp/"data")
    (tmp/"run").mkdir(parents=True, exist_ok=True)
    m = SourceFileLoader("slopbox", "/home/user/sarun/sarun").load_module()
    m.ensure_dirs()
    try:
        asyncio.run(drive(m))
    except Exception as e:
        import traceback; traceback.print_exc(); _fails.append(str(e))
    finally:
        mnt = tmp/"run"/"slopbox"/"mnt"
        try:
            if mnt.exists() and os.path.ismount(str(mnt)):
                subprocess.run(["fusermount3","-uz",str(mnt)],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                               timeout=10)
        except Exception: pass
        shutil.rmtree(tmp, ignore_errors=True)
    print("\n" + ("CHANGES-VIEW INCREMENTAL PASS" if not _fails
                  else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


if __name__ == "__main__":
    sys.exit(main())
