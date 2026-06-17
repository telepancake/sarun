#!/usr/bin/env python3
"""Docs coverage guard — ties the right-pane docs to the actual code surface.

The recurring failure mode is under-documentation: a real CLI subcommand, flag, or
UI keybinding exists in code but no `#:` doc block mentions it. This test makes that a
HARD FAILURE by extracting the user-facing surface straight from the source and asserting
each item appears in collect_docs(). Three layers:

  A. CLI subcommands + flags, regex-extracted from the argv dispatch / argparse setup.
  B. A curated list of fundamental concepts and corrected facts that must be explained.
  C. Drift guard: every `Binding(...)` declared in the source, by its description words,
     so adding a new bound action without documenting it fails here.

Run:  python3 test_docs_coverage.py
"""
import re, sys
from pathlib import Path
from importlib.machinery import SourceFileLoader

SARUN = Path(__file__).parent / "sarun"
src = SARUN.read_text()
m = SourceFileLoader("sarun", str(SARUN)).load_module()

# Docs as the user sees them, with Rich [markup] stripped.
docs = m.collect_docs()
plain = re.sub(r"\[/?[^\]]*\]", "", docs)
plain_low = plain.lower()

_fails = []
def need(token, *, label=None, ci=True):
    hay = plain_low if ci else plain
    needle = token.lower() if ci else token
    ok = needle in hay
    print(("  ok  " if ok else " FAIL ") + (label or repr(token)))
    if not ok: _fails.append(label or token)

# ── Layer A: CLI surface, extracted from code ────────────────────────────────
print("== CLI subcommands (from `argv[0] == \"...\"`) ==")
subcmds = sorted(set(re.findall(r'argv\[0\] == "([a-z]+)"', src)))
assert {"apply", "discard", "patch", "rename"} <= set(subcmds), subcmds
for sc in subcmds:
    need(sc, label=f"subcommand '{sc}' documented")

print("== CLI flags (from argparse add_argument) ==")
flags = sorted(set(re.findall(r'ap\.add_argument\("(-[a-zA-Z])"', src)))
assert {"-t", "-d", "-C"} <= set(flags), flags
for fl in flags:
    need(fl, label=f"flag '{fl}' documented", ci=False)
need("--", label="`--` command separator documented", ci=False)

# ── Layer B: fundamental concepts & corrected facts ──────────────────────────
print("== fundamental concepts ==")
CONCEPTS = [
    # the four real box states (killed/error were previously missing)
    "running", "finished", "killed", "error",
    # the four review/teardown ops are distinct things
    "kill", "delete", "apply all", "discard all", "refresh",
    # nesting — both the concept and how you start one
    "nest", "PARENT.CHILD",
    # capture model
    "copy-on-write", "overlay", "provenance", "direct",
    # file-rule actions
    "passthrough",
    # on-disk rule files users can edit
    "filerules",
]
for c in CONCEPTS:
    # PARENT.CHILD is the only case-sensitive token here.
    need(c, ci=(c != "PARENT.CHILD"))

# ── Layer C: drift guard — every Binding's action must be documented ──────────
print("== keybinding drift guard (every Binding description) ==")
bindings = re.findall(r'Binding\("([^"]+)"\s*,\s*"[^"]+"\s*,\s*"([^"]*)"', src)
STOP = {"a", "an", "the", "this", "to", "of"}
for key, desc in bindings:
    if not desc.strip():
        continue   # hidden / approval keys carry no description; covered by Layer B
    # Lenient: each meaningful word of the description must appear in the docs, so
    # rephrasing is allowed but dropping the action entirely is not.
    words = [w for w in re.split(r"\s+", desc.lower()) if w and w not in STOP]
    missing = [w for w in words if w not in plain_low]
    ok = not missing
    print(("  ok  " if ok else " FAIL ")
          + f"binding {key!r} ({desc!r})" + ("" if ok else f" — missing {missing}"))
    if not ok: _fails.append(f"binding {key} {desc}")

# ── structural: blocks are numbered and emitted in ascending order ───────────
print("== structure ==")
nums = [int(re.match(r"\s*(\d+)", re.sub(r"\[/?[^\]]*\]", "", b.split("\n", 1)[0])).group(1))
        for b in docs.split("\n\n") if re.match(r"\s*(?:\[[^\]]*\])*\s*\d", b)]
top = [n for n in nums]
# de-dupe consecutive (blocks with internal blank lines split on \n\n)
seen = []
for n in top:
    if not seen or seen[-1] != n: seen.append(n)
ascending = all(b >= a for a, b in zip(seen, seen[1:]))
print(("  ok  " if ascending else " FAIL ") + f"sections in ascending order: {seen}")
if not ascending: _fails.append("section ordering")

if _fails:
    print(f"\n{len(_fails)} FAILED: {_fails}")
    sys.exit(1)
print("\nall passed")
