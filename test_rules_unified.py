#!/usr/bin/env -S uv run --with pytest --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["textual>=0.60","mitmproxy>=11","wcmatch>=8.4","pyfuse3>=3.2",
#                 "trio>=0.22","python-magic>=0.4"]
# ///
"""Tests for the unified rule match model (one Match struct + box scope).

Covers three layers:
  1. the data model — parse/to_line round-trips (incl. the [BOXGLOB] scope and its
     backward-compatible absence), the shared box gate, the generic
     target.matches(Match) convention, and Rules/FileRules.decide threading the box;
  2. evaluation against a LIVE box — a box-scoped file passthrough rule over a real
     overlay mount, and a box-scoped network rule through a real Supervisor/policy,
     both honouring the box's hierarchical display name (incl. descendants);
  3. the UI — RuleFormModal composes a parseable line for either domain with the box
     scope, and a connection approval can save a box-scoped rule.

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


# ── 1 · data model ───────────────────────────────────────────────────────────
def test_model_parse_roundtrip_and_match():
    R, F, Match = m.Rule, m.FileRule, m.Match

    # legacy lines round-trip byte-identically; box defaults to ""
    r = R.parse("allow host:example.com")
    check(r.action == "allow" and r.kind == "host" and r.pattern == "example.com"
          and r.box == "", "net: legacy line parses")
    check(r.to_line() == "allow host:example.com", "net: legacy line round-trips unchanged")
    check(R.parse("! example.com").action == "deny", "net: legacy ! deny prefix")
    f = F.parse("discard **/__pycache__/**")
    check(f.box == "" and f.to_line() == "discard **/__pycache__/**",
          "file: legacy line round-trips unchanged")
    check(m.FileRule(action="passthrough", pattern="x").pattern == "x",
          "file: keyword constructor still works")

    # box scope round-trips
    check(R.parse("[backend-*] allow host:x").box == "backend-*", "net: box scope parsed")
    check(R.parse("[A.B*] deny ip:10.0.0.0/8").to_line() == "[A.B*] deny ip:10.0.0.0/8",
          "net: scoped line round-trips")
    check(F.parse("[A.B*] discard **/secret/**").to_line() == "[A.B*] discard **/secret/**",
          "file: scoped line round-trips")

    # a bare bracket (IPv6 literal / char-class glob) is NOT a box scope
    check(m._split_box("[::1]") == ("", "[::1]"), "split: bare [::1] is not a scope")
    check(m._split_box("[abc]*.txt") == ("", "[abc]*.txt"), "split: bare char-class is not a scope")
    check(m._split_box("[A.B*] allow host:x") == ("A.B*", "allow host:x"), "split: real scope")

    # shared box gate, matched with the same wcmatch engine as file globs
    check(Match("host", "x", "backend-*").box_ok("backend-1") is True, "box gate: matches")
    check(Match("host", "x", "backend-*").box_ok("frontend") is False, "box gate: rejects")
    check(Match("host", "x", "").box_ok("anything") is True, "box gate: empty = any box")
    check(Match("path", "*", "A.B*").box_ok("A.B.C") is True, "box gate: A.B* matches dotted A.B.C")

    # generic target.matches(Match): box gate first, then the kind facet
    ct = m.ConnTarget("example.com", 443, None, (), (), "backend-1")
    check(ct.matches(Match("host", "example.com", "backend-*")) is True, "ConnTarget: host+box")
    check(ct.matches(Match("host", "example.com", "frontend-*")) is False, "ConnTarget: box excludes")
    pt = m.PathTarget("etc/secret/key", "A.B")
    check(pt.matches(Match("path", "**/secret/**", "A.*")) is True, "PathTarget: path+box")
    check(pt.matches(Match("path", "**/secret/**", "Z.*")) is False, "PathTarget: box excludes")
    check(pt.matches(Match("host", "x")) is False, "PathTarget: ignores non-path kind")

    # the lists thread the box through decide()
    d = Path(tempfile.mkdtemp())
    try:
        nr = m.Rules(d / "n"); nr.rules = [R("deny", "host", "x.test", box="backend-*"),
                                           R("allow", "host", "*")]
        check(nr.decide("x.test", 443, box="backend-1") == "deny", "net decide: scoped deny applies")
        check(nr.decide("x.test", 443, box="frontend") == "allow", "net decide: scoped deny skipped")
        fr = m.FileRules(d / "f"); fr.rules = [F("passthrough", "s.txt", box="A.*"),
                                               F("discard", "*")]
        check(fr.decide("s.txt", box="A.B") == "passthrough", "file decide: scoped passthrough applies")
        check(fr.decide("s.txt") == "discard", "file decide: no box -> scoped rule skipped")
    finally:
        shutil.rmtree(d, ignore_errors=True)


# ── 2 · evaluation against a live box ────────────────────────────────────────
def test_box_scope_live_file_and_net():
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
        fr.rules = [m.FileRule(action="passthrough", pattern="secret.txt", box="ALPHA*")]
        ops.sessions[sid]["frules"] = fr

        mount.set_box_path(sid, "ALPHA.CHILD")
        check(ops._passthrough(sid, "secret.txt") is True,
              "file: scoped rule applies in ALPHA.CHILD")
        check(ops._passthrough(sid, "other.txt") is False, "file: non-matching path not passthrough")
        mount.set_box_path(sid, "BETA")
        check(ops._passthrough(sid, "secret.txt") is False, "file: scope excludes BETA")
        mount.set_box_path(sid, "")
        check(ops._passthrough(sid, "secret.txt") is False, "file: no box name -> scoped rule skipped")
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
    rules.rules = [m.Rule.parse("[ALPHA*] deny host:blocked.test"),
                   m.Rule.parse("allow host:*")]
    sup = m.Supervisor(rules)
    def mk(s, name, parent=None):
        sess = m.Session(session_id=s, box_id=int(s), name=name, cmd=["x"],
                         shm_dir=str(tmp / "live" / s), live=True, parent_box_id=parent)
        sup.sessions[s] = sess; return sess
    a = mk("10", "ALPHA"); b = mk("20", "BETA"); child = mk("11", "CHILD", parent=10)
    check(sup.display_path(11) == "ALPHA.CHILD", "net: nested display path A.CHILD")
    check(sup.policy.decide(a, "blocked.test", 443) == "deny", "net: scoped deny applies in ALPHA")
    check(sup.policy.decide(child, "blocked.test", 443) == "deny",
          "net: scoped deny applies in descendant ALPHA.CHILD")
    check(sup.policy.decide(b, "blocked.test", 443) == "allow", "net: scope excludes BETA")
    shutil.rmtree(tmp, ignore_errors=True)


# ── 3 · UI form + approval ───────────────────────────────────────────────────
def _freevar(fn, name):
    fv = fn.__code__.co_freevars
    return fn.__closure__[fv.index(name)].cell_contents if name in fv else None

def test_rule_form_modal_and_approval():
    App = m._make_ui_app()
    RuleFormModal = _freevar(App._file_rule_add, "RuleFormModal")
    check(RuleFormModal is not None, "RuleFormModal reachable")
    if RuleFormModal is None: return

    class _In:
        def __init__(self, v): self.value = v
    def compose(domain, action, pat, box, kind="host"):
        mod = RuleFormModal(domain)
        stubs = {"#rf-pat": _In(pat), "#rf-box": _In(box),
                 "#rf-action": _In(action), "#rf-kind": _In(kind)}
        object.__setattr__(mod, "query_one", lambda sel, cls=None: stubs[sel])
        return mod._line()

    check(compose("file", "discard", "**/*.log", "A.B*") == "[A.B*] discard **/*.log",
          "form: file composes scoped line")
    check(m.FileRule.parse(compose("file", "discard", "**/*.log", "A.B*")).box == "A.B*",
          "form: file line round-trips")
    check(compose("file", "passthrough", "s.txt", "") == "passthrough s.txt",
          "form: file no box -> no prefix")
    check(compose("net", "deny", "example.com", "backend-*") == "[backend-*] deny host:example.com",
          "form: net composes scoped line")
    check(compose("net", "allow", "10.0.0.0/8", "", kind="ip") == "allow ip:10.0.0.0/8",
          "form: net ip kind, no box")
    check(compose("file", "discard", "", "X") is None, "form: empty pattern -> no line")

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
    check([r.to_line() for r in rules.rules] == ["[backend-*] deny host:h.test"],
          "approval: saves a box-scoped rule")
    shutil.rmtree(d, ignore_errors=True)


def main():
    for fn in (test_model_parse_roundtrip_and_match,
               test_box_scope_live_file_and_net,
               test_rule_form_modal_and_approval):
        print(f"== {fn.__name__} ==")
        try:
            fn()
        except Exception as e:
            import traceback; traceback.print_exc(); _fails.append(f"{fn.__name__}: {e}")
    print("\n" + ("RULES-UNIFIED PASS" if not _fails else f"{len(_fails)} FAILURE(S)"))
    return 1 if _fails else 0


if __name__ == "__main__":
    sys.exit(main())
