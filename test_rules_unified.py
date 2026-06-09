#!/usr/bin/env -S uv run --with pytest --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["textual>=0.60","mitmproxy>=11","wcmatch>=8.4","pyfuse3>=3.2",
#                 "trio>=0.22","python-magic>=0.4"]
# ///
"""Tests for the unified rule model: an atomic Match(kind, pattern), a generic
and/or/not/enabled fold over a Clause list, and targets that only know how to test a
single Match. Covers three layers:

  1. the model — Match/Clause/eval_clauses, target.match_one, parse/to_line round-trips
     (incl. multi-clause and/or/not/off and the box kind), and Rules/FileRules.decide;
  2. evaluation against a LIVE box — a box-scoped file passthrough rule over a real
     overlay mount, and a multi-clause net rule through a real Supervisor/policy;
  3. the UI — the reusable ClauseList editor (add/remove rows, per-row kind/and-or/
     not/enabled) building a rule in a running app, and an approval saving a box rule.

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
    R, F, Match, Clause = m.Rule, m.FileRule, m.Match, m.Clause

    # atomic predicates + generic targets (each target tests ONE Match)
    ct = m.ConnTarget("api.example.com", 443, "http://api.example.com/x",
                      ("10.0.0.1",), ("cdn.example.com",), "backend-1")
    check(ct.match_one(Match("host", "*.example.com")) is True, "ConnTarget host")
    check(ct.match_one(Match("ip", "10.0.0.0/8")) is True, "ConnTarget ip/CIDR")
    check(ct.match_one(Match("cname", "cdn.*")) is True, "ConnTarget cname")
    check(ct.match_one(Match("url", "http://api.example.com/")) is True, "ConnTarget url prefix")
    check(ct.match_one(Match("box", "backend-*")) is True, "ConnTarget box")
    check(ct.match_one(Match("path", "x")) is False, "ConnTarget ignores path kind")
    pt = m.PathTarget("etc/secret/key", "A.B")
    check(pt.match_one(Match("path", "**/secret/**")) is True, "PathTarget path")
    check(pt.match_one(Match("box", "A.*")) is True, "PathTarget box")
    check(pt.match_one(Match("host", "x")) is False, "PathTarget ignores host kind")

    # generic and/or/not/enabled fold (target-agnostic)
    T = lambda v: types.SimpleNamespace(match_one=lambda mm, _v=v: _v.get(mm.kind, False))
    cl = lambda kind, join="and", negate=False, enabled=True: Clause(Match(kind, "*"), join, negate, enabled)
    check(m.eval_clauses(T({"host": True, "box": True}), [cl("host"), cl("box")]) is True, "fold: A and B")
    check(m.eval_clauses(T({"host": True, "box": False}), [cl("host"), cl("box")]) is False, "fold: A and not-B")
    check(m.eval_clauses(T({"host": True, "box": False}), [cl("host"), cl("box", join="or")]) is True, "fold: A or B")
    check(m.eval_clauses(T({"box": True}), [cl("host"), cl("box", negate=True)]) is False, "fold: not negates")
    check(m.eval_clauses(T({"host": True, "box": False}),
                         [cl("host"), cl("box", enabled=False)]) is True, "fold: disabled clause skipped")
    check(m.eval_clauses(T({}), []) is False, "fold: no enabled clauses -> matches nothing")

    # parse / to_line round-trips
    r = R.parse("allow host:example.com")
    check(len(r.clauses) == 1 and r.clauses[0].match.kind == "host"
          and r.clauses[0].match.pattern == "example.com", "net: single clause parses")
    check(r.to_line() == "allow host:example.com", "net: single clause round-trips")
    check(F.parse("discard **/*.log").to_line() == "discard **/*.log", "file: path renders bare")
    check(F.parse("discard **/*.log").clauses[0].match.kind == "path", "file: bare pattern -> path kind")

    multi = "deny host:api.* and not box:trusted or url:http://x/"
    rr = R.parse(multi)
    check([(c.match.kind, c.join, c.negate) for c in rr.clauses]
          == [("host", "and", False), ("box", "and", True), ("url", "or", False)],
          "net: multi-clause and/not/or parsed")
    check(rr.to_line() == multi, "net: multi-clause round-trips")
    fp = F.parse("apply off box:x and path:y")
    check(fp.clauses[0].enabled is False, "file: 'off' disables a clause")
    check(fp.to_line() == "apply off box:x and y", "file: disabled clause + bare path render")
    check(F.parse(fp.to_line()).to_line() == fp.to_line(), "file: to_line is idempotent")

    # file rules require an explicit action; net defaults to allow for a bare pattern
    check(F.parse("**/*.log") is None, "file: bare pattern with no action -> rejected")
    check(R.parse("example.com").to_line() == "allow host:example.com", "net: bare pattern -> allow host")
    hp = F.parse("discard host:foo")
    check(hp.clauses[0].match.kind == "path" and hp.clauses[0].match.pattern == "host:foo",
          "file: 'host:' is not a file kind -> treated as a literal path")

    # decide threads the box; multiple conditions are ANDed within one rule
    d = Path(tempfile.mkdtemp())
    try:
        nr = R.parse("deny host:x.test and box:backend-*")
        rules = m.Rules(d / "n"); rules.rules = [nr, R.parse("allow host:*")]
        check(rules.decide("x.test", 443, box="backend-1") == "deny", "net decide: host AND box both match")
        check(rules.decide("x.test", 443, box="frontend") == "allow", "net decide: box fails -> falls through")
        fr = m.FileRules(d / "f")
        fr.rules = [F.parse("passthrough secret.txt and box:A.*"), F.parse("discard *")]
        check(fr.decide("secret.txt", box="A.B") == "passthrough", "file decide: path AND box")
        check(fr.decide("secret.txt") == "discard", "file decide: no box -> scoped rule skipped")
    finally:
        shutil.rmtree(d, ignore_errors=True)


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

    rules = m.Rules(tmp / "netrules")
    rules.rules = [m.Rule.parse("deny host:blocked.test and box:ALPHA*"),
                   m.Rule.parse("allow host:*")]
    sup = m.Supervisor(rules)
    def mk(s, name, parent=None):
        sess = m.Session(session_id=s, box_id=int(s), name=name, cmd=["x"],
                         shm_dir=str(tmp / "live" / s), live=True, parent_box_id=parent)
        sup.sessions[s] = sess; return sess
    a = mk("10", "ALPHA"); b = mk("20", "BETA"); child = mk("11", "CHILD", parent=10)
    check(sup.display_path(11) == "ALPHA.CHILD", "net: nested display path")
    check(sup.policy.decide(a, "blocked.test", 443) == "deny", "net: host AND box deny in ALPHA")
    check(sup.policy.decide(child, "blocked.test", 443) == "deny", "net: applies in descendant ALPHA.CHILD")
    check(sup.policy.decide(b, "blocked.test", 443) == "allow", "net: box clause excludes BETA")
    shutil.rmtree(tmp, ignore_errors=True)


# ── 3 · UI form + approval ───────────────────────────────────────────────────
def _freevar(fn, name):
    fv = fn.__code__.co_freevars
    return fn.__closure__[fv.index(name)].cell_contents if name in fv else None

def test_approval_box_rule():
    # A connection approval can save a box-scoped rule (the box rides in as a clause).
    d = Path(tempfile.mkdtemp()); rules = m.Rules(d / "netrules"); pol = m.NetworkPolicy(rules)
    async def drive():
        sess = types.SimpleNamespace(box_id=1, sess_rules=[])
        pol._resolve_session = lambda sid: sess
        fut = asyncio.ensure_future(pol.approval_request("1", "h.test", 443, "https"))
        await asyncio.sleep(0.05)
        rid = next(iter(pol.pending))
        pol.resolve(rid, "deny", "permanent", spec="host:h.test", box="backend-*")
        await fut
    asyncio.run(drive())
    check([r.to_line() for r in rules.rules] == ["deny host:h.test and box:backend-*"],
          "approval: saves a box-scoped rule")
    shutil.rmtree(d, ignore_errors=True)


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
            modal = RuleFormModal("file")
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
            net = RuleFormModal("net"); app.push_screen(net, lambda r: None)
            await pilot.pause(); await pilot.pause()
            r0 = net.query(".clause-row")[0]
            r0.query_one("#cl-kind", Select).value = "host"
            r0.query_one("#cl-pat", Input).value = "api.test"
            await pilot.pause()
            nr = net._build()
            check(type(nr).__name__ == "Rule" and nr.to_line() == "allow host:api.test",
                  "the same editor builds a net Rule")
    asyncio.run(drive())


def main():
    for fn in (test_model, test_live_box_scope, test_approval_box_rule,
               test_clause_list_editor):
        print(f"== {fn.__name__} ==")
        try:
            fn()
        except Exception as e:
            import traceback; traceback.print_exc(); _fails.append(f"{fn.__name__}: {e}")
    print("\n" + ("RULES-UNIFIED PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


if __name__ == "__main__":
    sys.exit(main())
