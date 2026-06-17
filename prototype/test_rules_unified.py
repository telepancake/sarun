#!/usr/bin/env -S uv run --with pytest --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["textual>=0.60","wcmatch>=8.4","pyfuse3>=3.2",
#                 "trio>=0.22","python-magic>=0.4"]
# ///
"""Tests for the unified rule model: an atomic Match(kind, pattern), a generic
and/or/not/enabled fold over a Clause list, and targets that only know how to test a
single Match. Covers three layers:

  1. the model — Match/Clause/eval_clauses, target.match_one, parse/to_line round-trips
     (incl. multi-clause and/or/not/off and the box kind), and FileRules.decide;
  2. evaluation against a LIVE box — a box-scoped file passthrough rule over a real
     overlay mount;
  3. the UI — the reusable ClauseList editor (add/remove rows, per-row kind/and-or/
     not/enabled) building a file rule in a running app.

Run standalone (uv provisions deps):  ./test_rules_unified.py
"""
import os, sys, asyncio, tempfile, shutil, subprocess, types
from pathlib import Path
from importlib.machinery import SourceFileLoader

SARUN = str(Path(__file__).resolve().parent / "sarun")
m = SourceFileLoader("slopbox", SARUN).load_module()

_fails = []
def check(cond, msg):
    print(("  ok  " if cond else " FAIL ") + msg)
    if not cond: _fails.append(msg)


# ── 1 · model ────────────────────────────────────────────────────────────────
def test_model():
    F, Match, Clause = m.FileRule, m.Match, m.Clause

    # atomic predicates + generic targets (each target tests ONE Match)
    subj = m.Subject(box="backend-1", exe="/usr/bin/curl", cwd="/home/u",
                     argv=("curl", "--insecure", "https://x"))
    # shared process facets (exe/cwd/arg) on the file target
    pt = m.PathTarget("etc/secret/key", m.Subject(box="A.B", exe="/bin/sh", argv=("sh",)))
    check(pt.match_one(Match("path", "**/secret/**")) is True, "PathTarget path")
    check(pt.match_one(Match("box", "A.*")) is True, "PathTarget box")
    check(pt.match_one(Match("exe", "sh")) is True, "PathTarget shares the exe facet")
    pt2 = m.PathTarget("a/b", subj)
    check(pt2.match_one(Match("exe", "curl")) is True, "process exe: bare matches basename (path glob)")
    check(pt2.match_one(Match("exe", "/usr/bin/curl")) is True, "process exe: absolute anchors")
    check(pt2.match_one(Match("exe", "wget")) is False, "process exe: non-match")
    check(pt2.match_one(Match("cwd", "/home/*")) is True, "process cwd")
    check(pt2.match_one(Match("arg", "--insecure")) is True, "process arg: any argv element")
    check(pt2.match_one(Match("arg", "--quiet")) is False, "process arg: non-match")

    # generic and/or/not/enabled fold (target-agnostic)
    T = lambda v: types.SimpleNamespace(match_one=lambda mm, _v=v: _v.get(mm.kind, False))
    cl = lambda kind, join="and", negate=False, enabled=True: Clause(Match(kind, "*"), join, negate, enabled)
    check(m.eval_clauses(T({"path": True, "box": True}), [cl("path"), cl("box")]) is True, "fold: A and B")
    check(m.eval_clauses(T({"path": True, "box": False}), [cl("path"), cl("box")]) is False, "fold: A and not-B")
    check(m.eval_clauses(T({"path": True, "box": False}), [cl("path"), cl("box", join="or")]) is True, "fold: A or B")
    check(m.eval_clauses(T({"box": True}), [cl("path"), cl("box", negate=True)]) is False, "fold: not negates")
    check(m.eval_clauses(T({"path": True, "box": False}),
                         [cl("path"), cl("box", enabled=False)]) is True, "fold: disabled clause skipped")
    check(m.eval_clauses(T({}), []) is False, "fold: no enabled clauses -> matches nothing")

    # parse / to_line round-trips
    check(F.parse("discard **/*.log").to_line() == "discard **/*.log", "file: path renders bare")
    check(F.parse("discard **/*.log").clauses[0].match.kind == "path", "file: bare pattern -> path kind")

    fp = F.parse("apply off box:x and path:y")
    check(fp.clauses[0].enabled is False, "file: 'off' disables a clause")
    check(fp.to_line() == "apply off box:x and y", "file: disabled clause + bare path render")
    check(F.parse(fp.to_line()).to_line() == fp.to_line(), "file: to_line is idempotent")

    multi = "apply path:a/* and not box:trusted or y"
    fm = F.parse(multi)
    check([(c.match.kind, c.join, c.negate) for c in fm.clauses]
          == [("path", "and", False), ("box", "and", True), ("path", "or", False)],
          "file: multi-clause and/not/or parsed")

    # file rules require an explicit action for a bare pattern
    check(F.parse("**/*.log") is None, "file: bare pattern with no action -> rejected")
    hp = F.parse("discard host:foo")
    check(hp.clauses[0].match.kind == "path" and hp.clauses[0].match.pattern == "host:foo",
          "file: 'host:' is not a file kind -> treated as a literal path")

    # decide threads the box; multiple conditions are ANDed within one rule
    d = Path(tempfile.mkdtemp())
    try:
        fr = m.FileRules(d / "f")
        fr.rules = [F.parse("passthrough secret.txt and box:A.*"), F.parse("discard *")]
        check(fr.decide("secret.txt", box="A.B") == "passthrough", "file decide: path AND box")
        check(fr.decide("secret.txt") == "discard", "file decide: no box -> scoped rule skipped")
        # process facets thread through decide() — file matches the writing process.
        fpr = m.FileRules(d / "fp")
        fpr.rules = [F.parse("passthrough *.key and arg:--export")]
        check(fpr.decide("a.key", proc={"argv": ["gpg", "--export"]}) == "passthrough",
              "file decide: matches the writer's argv")
        check(fpr.decide("a.key", proc={"argv": ["gpg", "--list"]}) is None,
              "file decide: writer argv no match")
    finally:
        shutil.rmtree(d, ignore_errors=True)


# ── 1b · per-entry FILTER targets + the internal "ids" kind ──────────────────
def test_filter_targets():
    """The list-filter targets each test their own domain kinds, the shared box/exe/cwd/
    arg facets, and the INTERNAL "ids" kind (a comma-separated set of process ROW ids,
    never a user kind) against the row id(s) the entry carries."""
    Match = m.Match

    # _ids_of parses a comma list into an int set; junk is dropped, never raises.
    check(m._ids_of("5,7") == {5, 7}, "_ids_of parses a comma list")
    check(m._ids_of(" 5 , 7 ,") == {5, 7}, "_ids_of trims and skips empties")
    check(m._ids_of("5,x,7") == {5, 7}, "_ids_of skips non-numeric tokens")
    check(m._ids_of("") == set() and m._ids_of(None) == set(), "_ids_of empty -> empty set")
    # "ids" is INTERNAL — never offered as a user kind.
    for kinds in (m.FILE_KINDS, m.SUBJECT_KINDS):
        check("ids" not in kinds, f"'ids' absent from {kinds!r}")

    subj = m.Subject(box="backend-1", exe="/usr/bin/curl", cwd="/home/u",
                     argv=("curl", "--insecure"))

    # Changes entry: path + shared facets + ids = {first,last writer}.
    pt = m.PathTarget("etc/secret/key", subj, ids=(5, 7))
    check(pt.match_one(Match("path", "**/secret/**")) is True, "PathTarget path")
    check(pt.match_one(Match("box", "backend-*")) is True, "PathTarget box facet")
    check(pt.match_one(Match("exe", "curl")) is True, "PathTarget exe facet")
    check(pt.match_one(Match("ids", "7")) is True, "PathTarget ids: last writer matches")
    check(pt.match_one(Match("ids", "5,99")) is True, "PathTarget ids: first writer matches")
    check(pt.match_one(Match("ids", "99")) is False, "PathTarget ids: no overlap -> False")
    check(m.PathTarget("a", subj).match_one(Match("ids", "5")) is False,
          "PathTarget with no ids never matches the ids kind")

    # Process entry: shared facets + ids = {own row id}.
    prt = m.ProcFilterTarget(7, subj)
    check(prt.match_one(Match("exe", "/usr/bin/curl")) is True, "ProcFilterTarget exe")
    check(prt.match_one(Match("cwd", "/home/*")) is True, "ProcFilterTarget cwd")
    check(prt.match_one(Match("arg", "--insecure")) is True, "ProcFilterTarget arg")
    check(prt.match_one(Match("box", "backend-*")) is True, "ProcFilterTarget box")
    check(prt.match_one(Match("ids", "5,7")) is True, "ProcFilterTarget ids: own row matches")
    check(prt.match_one(Match("ids", "5,8")) is False, "ProcFilterTarget ids: own row absent -> False")

    # eval_clauses folds an ids clause exactly like any other (used for navigation).
    Clause = m.Clause
    cl = [Clause(Match("ids", "7"))]
    check(m.eval_clauses(m.ProcFilterTarget(7, subj), cl) is True, "ids fold: matching row kept")
    check(m.eval_clauses(m.ProcFilterTarget(8, subj), cl) is False, "ids fold: other row dropped")


# ── 2 · evaluation against a live box ────────────────────────────────────────
def test_live_box_scope():
    tmp = Path(tempfile.mkdtemp(prefix="boxscope-"))
    os.environ["XDG_STATE_HOME"] = str(tmp / "state")
    os.environ["XDG_CONFIG_HOME"] = str(tmp / "cfg")
    mnt = tmp / "mnt"; sid = "1"; backing = tmp / "live" / sid
    (backing / "up").mkdir(parents=True)
    index = m.Index(backing); mount = m.OverlayMount(mnt, lower="/")
    if not mount.start():
        check(False, f"overlay mount failed: {mount._start_error}"); return
    mount.add_session(sid, backing / "up", index, passthrough=False)
    ops = mount.ops
    try:
        fr = m.FileRules.__new__(m.FileRules); fr.path = None
        fr.rules = [m.FileRule.parse("passthrough secret.txt and box:ALPHA*")]
        ops.sessions[sid]["frules"] = fr
        mount.set_box_path(sid, "ALPHA.CHILD")
        check(ops._passthrough(sid, "secret.txt") is True, "file: scoped rule applies in ALPHA.CHILD")
        check(ops._passthrough(sid, "other.txt") is False, "file: non-matching path not passthrough")
        mount.set_box_path(sid, "BETA")
        check(ops._passthrough(sid, "secret.txt") is False, "file: box clause excludes BETA")
        mount.set_box_path(sid, "")
        check(ops._passthrough(sid, "secret.txt") is False, "file: no box name -> box clause fails")
    finally:
        try: mount.stop()
        except Exception: pass
        try:
            if os.path.ismount(str(mnt)):
                subprocess.run(["fusermount3", "-uz", str(mnt)],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)
        except Exception: pass
        index.close()
    shutil.rmtree(tmp, ignore_errors=True)


# ── 3 · UI form ──────────────────────────────────────────────────────────────
def _freevar(fn, name):
    fv = fn.__code__.co_freevars
    return fn.__closure__[fv.index(name)].cell_contents if name in fv else None

def test_process_facet_live():
    """End to end: a real write records the path's FIRST writer; a passthrough rule with
    a process facet (exe/arg) matches that recorded writer, and a non-matching facet
    doesn't — the 'locked to the first writer' behaviour. Also exercises the stopped-box
    reader sqlar_first_writer_prov."""
    tmp = Path(tempfile.mkdtemp(prefix="procmatch-"))
    os.environ["XDG_STATE_HOME"] = str(tmp / "state")
    os.environ["XDG_CONFIG_HOME"] = str(tmp / "cfg")
    mnt = tmp / "mnt"; sid = "1"; backing = tmp / "live" / sid
    (backing / "up").mkdir(parents=True)
    index = m.Index(backing); mount = m.OverlayMount(mnt, lower="/")
    if not mount.start():
        check(False, f"overlay mount failed: {mount._start_error}"); return
    mount.add_session(sid, backing / "up", index, passthrough=False)
    ops = mount.ops; root = mnt / sid
    try:
        # a real captured write through the mount records bash as the path's first writer
        r = subprocess.run(["bash", "-c", "echo hi > note.txt"], cwd=str(root),
                           capture_output=True, text=True)
        check(r.returncode == 0, f"captured write succeeded (err={r.stderr!r})")
        prov = index.first_writer_provenance("note.txt")
        check(prov is not None and prov.get("exe", "").endswith("bash")
              and prov.get("argv") and prov.get("cwd"),
              f"first_writer_provenance returns the writer exe/cwd/argv (got {prov})")

        fr = m.FileRules.__new__(m.FileRules); fr.path = None
        ops.sessions[sid]["frules"] = fr
        fr.rules = [m.FileRule.parse("passthrough note.txt and exe:**/bash")]
        check(ops._passthrough(sid, "note.txt") is True,
              "passthrough rule matches the recorded first-writer exe")
        fr.rules = [m.FileRule.parse("passthrough note.txt and exe:**/python3")]
        check(ops._passthrough(sid, "note.txt") is False,
              "passthrough rule with a non-matching exe does not fire (locked to first writer)")
        fr.rules = [m.FileRule.parse("passthrough note.txt and arg:-c")]
        check(ops._passthrough(sid, "note.txt") is True,
              "passthrough rule matches an argv element of the first writer")
        fr.rules = [m.FileRule.parse("passthrough note.txt and cwd:**")]
        check(ops._passthrough(sid, "note.txt") is True,
              "passthrough rule matches the first writer's recorded cwd")
    finally:
        try: mount.stop()
        except Exception: pass
        try:
            if os.path.ismount(str(mnt)):
                subprocess.run(["fusermount3", "-uz", str(mnt)],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=10)
        except Exception: pass
        index.close()
    shutil.rmtree(tmp, ignore_errors=True)


def test_clause_list_editor():
    """Drive the REAL reusable ClauseList editor in a running app: it starts with one
    row, 'add condition' mounts another, each row's kind/pattern/join/not/enabled read
    back into a Clause, the modal builds the domain's rule from them, removing a row
    drops its clause, and a rule with no live condition is refused."""
    App = m._make_ui_app()
    RuleFormModal = _freevar(App._file_rule_add, "RuleFormModal")

    async def drive():
        from textual.widgets import Input, Select
        app = App()
        async with app.run_test(size=(120, 40)) as pilot:
            await pilot.pause()
            modal = RuleFormModal()
            app.push_screen(modal, lambda r: None)
            await pilot.pause(); await pilot.pause()
            cl = modal.query_one("#rf-clauses")
            rows = list(modal.query(".clause-row"))
            check(len(rows) == 1, "editor starts with one clause row")
            check(rows[0].query_one("#cl-kind", Select).value == "path",
                  "file row defaults to the path kind")
            rows[0].query_one("#cl-pat", Input).value = "**/*.log"
            cl._add(types.SimpleNamespace(stop=lambda: None))
            await pilot.pause(); await pilot.pause()
            rows = list(modal.query(".clause-row"))
            check(len(rows) == 2, "'add condition' mounts a second row")
            rows[1].query_one("#cl-kind", Select).value = "box"
            rows[1].query_one("#cl-pat", Input).value = "A.B*"
            await pilot.pause()
            check([(c.match.kind, c.match.pattern) for c in cl.clauses()]
                  == [("path", "**/*.log"), ("box", "A.B*")], "clauses() reads both rows in order")
            rule = modal._build()
            check(type(rule).__name__ == "FileRule"
                  and rule.to_line() == "apply **/*.log and box:A.B*",
                  f"modal builds the domain rule from the rows (got {rule and rule.to_line()!r})")
            rows[1]._remove(types.SimpleNamespace(stop=lambda: None))
            await pilot.pause(); await pilot.pause()
            check(len(cl.clauses()) == 1, "removing a row drops its clause")
            list(modal.query(".clause-row"))[0].query_one("#cl-pat", Input).value = ""
            await pilot.pause()
            check(modal._build() is None, "a rule with no live condition is refused")
            modal.dismiss(None); await pilot.pause()
    asyncio.run(drive())


def test_list_filtering():
    """Drive the REAL app's procs pane and assert the '/' filter (and a generated "ids"
    filter) renders exactly the matching subset. Stubs the Supervisor's data getters so
    the pane is populated deterministically (no live box / fuse), then sets the view's
    filter state and forces a reload, reading back the rendered DataTable row keys."""
    from textual.widgets import DataTable
    App = m._make_ui_app()
    SearchModal = _freevar(App.action_filter, "SearchModal")
    check(SearchModal is not None, "SearchModal reachable from action_filter closure")

    sid = "1"
    # Four processes: a root bash, a curl, a wget, a python — distinct row ids.
    procs = [(1, 10, 0, None, "/bin/bash", ["bash"]),
             (2, 11, 10, 1, "/usr/bin/curl", ["curl", "http://x"]),
             (3, 12, 10, 1, "/usr/bin/wget", ["wget", "http://y"]),
             (4, 13, 10, 1, "/usr/bin/python3", ["python3", "s.py"])]
    cwds = {1: "/root", 2: "/home/u", 3: "/home/u", 4: "/srv"}

    async def drive():
        app = App()
        async with app.run_test(size=(120, 40)) as pilot:
            await pilot.pause()
            # Stub the data layer: a finished box (processes_live None) with our table.
            app.sup.proc_roots = lambda s: {1}
            app.sup.processes_live = lambda s: None
            app.sup.processes = lambda s: list(procs)
            app.sup.proc_prov = lambda s, rid: dict(
                exe=dict((p[0], p[4]) for p in procs).get(rid, ""),
                cwd=cwds.get(rid, ""),
                argv=dict((p[0], p[5]) for p in procs).get(rid, []))
            app.sup.review._live = lambda s: False
            app._maybe_start_consolidate = lambda s: False
            app.sessions = {sid: dict(session_id=sid, name="BOX", path="BOX")}
            app._sel_sid = sid
            app.view = "procs"

            def rendered():
                t = app.query_one("#pr-tab", DataTable)
                return [rk.value for rk in t.rows]

            app._load_procs(sid, force=True)
            await pilot.pause()
            check(set(rendered()) == {"1", "2", "3", "4"},
                  f"unfiltered procs pane shows all rows (got {rendered()})")

            # User filter: exe glob keeping only curl + wget.
            app._view_filters["procs"] = {"clauses": [m.Clause(m.Match("exe", "**/{curl,wget}"))],
                                     "on": True, "generated": False}
            app._load_procs(sid, force=True)
            await pilot.pause()
            check(set(rendered()) == {"2", "3"},
                  f"exe filter keeps only curl+wget (got {rendered()})")

            # cwd facet narrows to one of them.
            app._view_filters["procs"] = {"clauses": [m.Clause(m.Match("cwd", "/srv")),
                                                 m.Clause(m.Match("exe", "**/python3"), join="and")],
                                     "on": True, "generated": False}
            app._load_procs(sid, force=True)
            await pilot.pause()
            check(set(rendered()) == {"4"}, f"cwd+exe filter keeps python (got {rendered()})")

            # GENERATED "ids" filter (what c/t/p navigation builds) selects exact rows.
            app._view_filters["procs"] = {"clauses": [m.Clause(m.Match("ids", "2,4"))],
                                     "on": True, "generated": True}
            app._load_procs(sid, force=True)
            await pilot.pause()
            check(set(rendered()) == {"2", "4"},
                  f"generated ids filter selects exactly rows 2,4 (got {rendered()})")

            # esc cancels a generated filter in place -> full list, filter off.
            app.action_back()
            await pilot.pause()
            check(set(rendered()) == {"1", "2", "3", "4"}
                  and app._view_filters["procs"]["on"] is False,
                  f"esc clears the generated filter -> full list (got {rendered()})")

            # Filtering OFF -> '/' opens the SearchModal seeded with no clauses; its kinds
            # are the procs vocabulary WITHOUT the internal ids kind.
            app.view = "procs"
            app.action_filter()
            await pilot.pause(); await pilot.pause()
            modal = app.screen
            check(isinstance(modal, SearchModal), "/' opens the search modal when off")
            kinds = _freevar(App.action_filter, "SearchModal") and modal._kinds
            check("ids" not in modal._kinds and set(modal._kinds) == set(m.SUBJECT_KINDS),
                  f"procs search offers SUBJECT_KINDS without ids (got {modal._kinds})")
            modal.dismiss(None); await pilot.pause()

            # Full c→p navigation: from changes, _nav("procs") sets the generated ids
            # filter and the procs pane renders exactly the writer rows.
            app.sup.writer_id = lambda s, rel: 2
            app.sup.first_writer_id = lambda s, rel: 4
            app._sel_path = lambda: "x.txt"
            app.view = "changes"
            app._nav("procs")
            await pilot.pause()
            st = app._view_filters["procs"]
            check(st["on"] and st["generated"] and app.view == "procs",
                  "c→p sets a generated filter and switches view")
            check(set(rendered()) == {"2", "4"},
                  f"c→p shows exactly the change's writers (got {rendered()})")
            # A second nav whose source yields no ids drops the stale generated filter.
            app._sel_path = lambda: None
            app.view = "changes"
            app._nav("procs")
            await pilot.pause()
            check(app._view_filters["procs"]["on"] is False
                  and set(rendered()) == {"1", "2", "3", "4"},
                  f"c→p with no writer clears the stale generated filter (got {rendered()})")

    asyncio.run(drive())


def test_nav_ids():
    """The c/p/o cross-navigation resolver builds the right "ids" filter: changes→procs
    pins to the change's first+last writer, and procs→changes/outputs to the selected
    process row. Tested at the resolver level (_nav_ids) with the selection getters +
    supervisor stubbed."""
    import types as _t
    App = m._make_ui_app()
    app = App.__new__(App)               # no Textual mount: exercise pure logic
    app.view = ""; app._sel_sid = "1"
    sup = _t.SimpleNamespace(
        first_writer_id=lambda s, rel: 5, writer_id=lambda s, rel: 7)
    app.sup = sup
    app._sel_path = lambda: "a/b.txt"
    app._sel_proc = lambda: 42
    app._sel_output = lambda: 3
    app._output_pid = lambda oid: 9

    app.view = "changes"
    check(app._nav_ids("changes", "procs") == [5, 7],
          "changes→procs: first + last writer ids")
    sup.first_writer_id = lambda s, rel: 7   # first==last: de-duped to one
    check(app._nav_ids("changes", "procs") == [7], "changes→procs: dedups equal writers")
    app.view = "procs"
    check(app._nav_ids("procs", "changes") == [42], "procs→changes: selected proc row")
    check(app._nav_ids("procs", "outputs") == [42], "procs→outputs: selected proc row")
    app.view = "outputs"
    check(app._nav_ids("outputs", "procs") == [9], "outputs→procs: write's process")
    # transitions that don't auto-filter
    check(app._nav_ids("changes", "outputs") is None, "changes→outputs: no auto-filter")
    check(app._nav_ids("procs", "procs") is None, "same-view: no auto-filter")
    app._sel_sid = None
    check(app._nav_ids("changes", "procs") is None, "no selected box: no filter")


def test_process_identity():
    """Process identity is per-incarnation (tgid,start) — 16-bit PIDs roll over during a
    big build, so a tgid alone cannot identify a process. Asserts: (1) two rows with the
    SAME tgid but DIFFERENT start_time are DISTINCT rows; (2) a child's parent_id is the
    parent's ROW id (not its tgid); (3) build_proc_tree links them via row ids."""
    tmp = Path(tempfile.mkdtemp(prefix="procid-"))
    os.environ["XDG_STATE_HOME"] = str(tmp / "state")
    os.environ["XDG_CONFIG_HOME"] = str(tmp / "cfg")
    sid = "1"; backing = tmp / "live" / sid
    (backing / "up").mkdir(parents=True)
    # No /proc reads: feed explicit (tgid,start) + parent_pid through process_from_prov.
    # Stub read_provenance/tgid_of so the synthetic parent walk is deterministic.
    PARENT, CHILD = 5000, 5001
    orig_rp, orig_tg, orig_st = m.read_provenance, m.tgid_of, m._proc_start_time
    m.tgid_of = lambda pid: int(pid or 0)
    m._proc_start_time = lambda pid: 0          # unknown unless explicitly supplied
    m.read_provenance = lambda pid, full_env=False: dict(
        ppid=0, exe="/bin/parent", argv=["parent"], env={}, start_time=111)
    try:
        idx = m.Index(backing)
        # The parent (root incarnation) at start 111.
        rid_parent = idx.process_from_prov(
            dict(tgid=PARENT, start_time=111, ppid=0, exe="/bin/parent",
                 argv=["parent"], env={}), root=True)
        # Child incarnation A: pid CHILD, start 222, parented to PARENT.
        rid_a = idx.process_from_prov(
            dict(tgid=CHILD, start_time=222, ppid=PARENT, parent_pid=PARENT,
                 exe="/bin/a", argv=["a"], env={}))
        # Child incarnation B: SAME pid CHILD reused, start 333 → a DISTINCT row.
        rid_b = idx.process_from_prov(
            dict(tgid=CHILD, start_time=333, ppid=PARENT, parent_pid=PARENT,
                 exe="/bin/b", argv=["b"], env={}))

        check(rid_a != rid_b,
              f"same tgid + different start_time = distinct rows ({rid_a} != {rid_b})")
        sp = m.sqlar_path(sid)
        rows = m.process_list(sp)
        child_rows = [r for r in rows if r[1] == CHILD]
        check(len(child_rows) == 2,
              f"two persisted process rows for the one reused tgid (got {len(child_rows)})")
        # parent_id is the parent's ROW id, NOT its tgid.
        for r in child_rows:
            rid, tgid, ppid, parent_id, exe, argv = r
            check(parent_id == rid_parent,
                  f"child {rid} parent_id is the parent's ROW id {rid_parent} (got {parent_id})")
            check(parent_id != PARENT,
                  "parent_id is a row id, not the parent's tgid/pid")

        # build_proc_tree links by row id: parent appears once, both child incarnations
        # are distinct nodes under it.
        tree = m.build_proc_tree(rows, m.process_roots(sp), None)
        by_rid = {t[0]: t for t in tree}
        check(rid_parent in by_rid and rid_a in by_rid and rid_b in by_rid,
              "tree contains the parent and both child incarnations as distinct nodes")
        # both children are one level below the (depth-0) parent.
        check(by_rid[rid_parent][5] == 0, "parent is a depth-0 root node")
        check(by_rid[rid_a][5] == 1 and by_rid[rid_b][5] == 1,
              "both child incarnations are depth-1 children of the parent")

        # PARENT pid reuse, child recorded FIRST: a parent's pid is reused (new start)
        # and a child of the NEW parent is recorded before the new parent itself. The
        # parent link must read /proc and bind the NEW incarnation, never the stale
        # cached row (the gap a naive _proc_current trust would leave).
        REUSE = 6000
        idx.process_from_prov(dict(tgid=REUSE, start_time=111, exe="/bin/old",
                                   argv=["old"], env={}))
        stale_row = idx._current_row(REUSE)
        m._proc_start_time = lambda pid: 222 if pid == REUSE else 0
        m.read_provenance = lambda pid, full_env=False: dict(
            ppid=0, exe="/bin/new", argv=["new"], env={}, start_time=222)
        fresh_row = idx._resolve_parent(REUSE)
        check(fresh_row is not None and fresh_row != stale_row,
              f"parent pid reuse (child-first): links the NEW incarnation, not the stale "
              f"row (stale={stale_row}, fresh={fresh_row})")
        check(idx._current_row(REUSE) == fresh_row,
              "current incarnation advances to the new parent row")
        idx.close()
    finally:
        m.read_provenance, m.tgid_of, m._proc_start_time = orig_rp, orig_tg, orig_st
        shutil.rmtree(tmp, ignore_errors=True)


def main():
    for fn in (test_model, test_filter_targets, test_live_box_scope,
               test_process_facet_live, test_process_identity, test_clause_list_editor,
               test_list_filtering, test_nav_ids):
        print(f"== {fn.__name__} ==")
        try:
            fn()
        except Exception as e:
            import traceback; traceback.print_exc(); _fails.append(f"{fn.__name__}: {e}")
    print("\n" + ("RULES-UNIFIED PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


if __name__ == "__main__":
    sys.exit(main())
