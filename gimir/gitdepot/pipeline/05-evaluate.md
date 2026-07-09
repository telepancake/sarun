# Agent 5 — Evaluate on real repositories

Read `gitdepot/pipeline/00-shared-context.md` and `gitdepot/VALIDATION.md`.

Measure the DESIGN-faithful union / reverse-delta engine, reading bytes BACK
from stored VBF frames — NEVER a git-side re-derivation, NEVER excluding any
frame as a "cache". Count what is actually written to disk.

Repos: `/home/user/sarun` itself (git pack size via `git count-objects -v`
`size-pack`, over the `refs/heads` + `refs/tags` closure), and one more real
repo if one is available locally.

Measure and report, honestly:
- ROUNDTRIP: reconstruct a sample of refs/commits from stored bytes and assert
  byte-exact equality with `git rev-parse <c>^{tree}`. Report match counts. Any
  mismatch is a HARD FAIL — report it, do not average it away.
- SIZE: total stored bytes (all frames as actually written) vs the git pack;
  the honest ratio. Measure SINGLE-LANE vs MULTI-LANE packing — the design's
  central claim is that multi-lane union packing beats packfiles on cross-branch
  redundancy. Report which wins and by how much.
- MEMORY: peak RSS during import; confirm it is BOUNDED and does not scale with
  full-state size (on-demand oid traversal keeps only the frontier + the oid
  tree cache, not the whole union).
- SCALING: import time and stored size vs number of commits.

Write `gitdepot/EVALUATION.md` with a numbers table and an HONEST bottom line —
no spin, no goalpost-moving. If it loses to git, say by how much and why. Do NOT
modify source, do NOT commit. Return a 15-line summary with the key numbers.
