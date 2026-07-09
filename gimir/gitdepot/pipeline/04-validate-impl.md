# Agent 4 — Validate implementation against the design (adversarial)

Read `gitdepot/pipeline/00-shared-context.md` and the authoritative design
`gitdepot/DESIGN-RECOVERED.md`. Then inspect the current code.

Go point by point through the design and mark IMPLEMENTED / PARTIAL / MISSING
with `file:symbol` evidence. Be adversarial — your job is to catch any claim of
"done" that is actually a shortcut or a second demo. Confirm specifically, each
a hard FAIL if not met:
- The REAL git ingest persists the union/lane encoding (grep the ingest path;
  plain per-tree encoding = FAIL).
- git tree traversal is by-oid-on-demand with the oid tree cache and O(changed)
  subtree pruning; whole-tree materialization into flat maps = FAIL.
- Storage is REVERSE-delta (f0 = tip, O(1) tip read); forward delta = FAIL.
- Historical refs served SHA-exact from STORED bytes on REAL git, multi-lane.
- reslot uses git (mode,oid) with the slot algebra; hashing stored content to
  reslot = FAIL.
- No design-faithful code was deleted or bypassed in favor of a reimplementation;
  the two-path split is resolved onto the design-faithful path.

Update `gitdepot/VALIDATION.md` with the point-by-point table and a blunt
verdict. Do NOT modify source, do NOT commit. Return a 12-line summary: what is
now real, what is still a gap, and whether the design-faithful union +
reverse-delta pipeline exists end-to-end on real git.
