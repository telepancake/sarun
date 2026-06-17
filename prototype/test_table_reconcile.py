#!/usr/bin/env python3
"""Headless tests for reconcile_datatable() — the generic incremental DataTable
reconciler that replaces the clear()+re-add() rebuild behind the changes / process
views.  Proves the properties the old path violated:

  * surviving rows are NEVER recreated (same Row object identity) — i.e. no rebuild;
  * arbitrary inserts/removals/reorders end in the exact desired order;
  * the cursor stays on the SAME row key (never thrown across the table);
  * the cursor row keeps its on-screen position while rows are inserted above it.

Run with the venv python (has textual):
    /home/user/venv/bin/python test_table_reconcile.py
"""
import sys
from importlib.machinery import SourceFileLoader

SARUN = "/home/user/sarun/prototype/sarun"
m = SourceFileLoader("slopbox", SARUN).load_module()

from textual.app import App
from textual.widgets import DataTable
from textual.widgets._data_table import RowKey
from rich.text import Text

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


def reconcile(dt, keys, tag="v", **kw):
    cells = {k: [Text(k), Text(f"{tag}{k}")] for k in keys}
    return m.reconcile_datatable(dt, keys, cells.__getitem__, **kw)


class TApp(App):
    def compose(self):
        yield DataTable()
    def on_mount(self):
        self.query_one(DataTable).add_columns("name", "val")


async def _run():
    app = TApp()
    async with app.run_test(size=(40, 12)) as pilot:
        dt = app.query_one(DataTable)

        # ── initial population ──────────────────────────────────────────────
        keys0 = [f"{i:03d}" for i in range(0, 60)]
        reconcile(dt, keys0)
        await pilot.pause()
        check(dt.row_count == 60, "initial: 60 rows present")
        check([r.key.value for r in dt.ordered_rows] == keys0,
              "initial: display order matches desired")

        # capture Row object identities to prove no recreation later
        ident_before = {rk.value: id(dt.rows[rk]) for rk in dt.rows}

        # put the cursor on a known key and scroll so it sits mid-viewport
        dt.move_cursor(row=dt.get_row_index("030"), animate=False)
        await pilot.pause()
        cur_key = dt.coordinate_to_cell_key((dt.cursor_row, 0)).row_key.value
        screen_y = dt.cursor_row - int(dt.scroll_offset.y)
        check(cur_key == "030", "cursor parked on key 030")

        # ── insert many rows ABOVE the cursor + some below, remove a few ─────
        new_above = [f"00{c}" for c in "abcde"]      # sort before "010"
        new_below = [f"05{c}" for c in "xyz"]        # sort after  "050"
        keys1 = sorted(set(keys0) - {"005", "006", "045"} | set(new_above) | set(new_below))
        added = reconcile(dt, keys1)
        await pilot.pause()

        check([r.key.value for r in dt.ordered_rows] == keys1,
              "after churn: display order exactly matches desired")
        check(set(added) == set(new_above) | set(new_below),
              f"after churn: returned added keys correct (got {sorted(added)})")
        check("005" not in {rk.value for rk in dt.rows}
              and "045" not in {rk.value for rk in dt.rows},
              "after churn: removed keys are gone")

        # NO REBUILD: survivors keep their exact Row object identity
        survivors = [k for k in keys0 if k in set(keys1)]
        same = all(k in {rk.value for rk in dt.rows}
                   and id(dt.rows[RowKey(k)]) == ident_before[k]
                   for k in survivors)
        check(same, "no rebuild: every surviving row kept its Row object identity")

        # CURSOR STABILITY: still on key 030, still at the same screen line
        cur_key2 = dt.coordinate_to_cell_key((dt.cursor_row, 0)).row_key.value
        screen_y2 = dt.cursor_row - int(dt.scroll_offset.y)
        check(cur_key2 == "030",
              f"cursor still on key 030 after 5 inserts above it (got {cur_key2})")
        check(screen_y2 == screen_y,
              f"cursor row held its on-screen position (was {screen_y}, now {screen_y2})")

        # ── removing the cursor's own row must not crash; cursor stays valid ─
        keys2 = [k for k in keys1 if k != "030"]
        reconcile(dt, keys2)
        await pilot.pause()
        check("030" not in {rk.value for rk in dt.rows}, "cursor row removed cleanly")
        check(dt.is_valid_coordinate(dt.cursor_coordinate),
              "cursor remains at a valid coordinate after its row vanished")

        # ── steady cell-only update keeps identities and order ──────────────
        ident_now = {rk.value: id(dt.rows[rk]) for rk in dt.rows}
        reconcile(dt, keys2, tag="W", update_cells=True)
        await pilot.pause()
        check([r.key.value for r in dt.ordered_rows] == keys2,
              "cell-only update: order unchanged")
        check(all(id(dt.rows[RowKey(k)]) == ident_now[k] for k in keys2),
              "cell-only update: no row recreated")
        check(str(dt.get_cell("031", list(dt.columns)[1])) == "W031",
              "cell-only update: changed cell value applied in place")


if __name__ == "__main__":
    import asyncio
    asyncio.run(_run())
    print()
    if _fails:
        print(f"{len(_fails)} FAILURE(S)"); sys.exit(1)
    print("ALL OK")
