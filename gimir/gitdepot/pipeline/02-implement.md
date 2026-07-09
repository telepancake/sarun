# Agent 2 — Implement per design

Read `gitdepot/pipeline/00-shared-context.md`, then the authoritative design
`gitdepot/DESIGN-RECOVERED.md`, then the work map `gitdepot/WORKMAP.md`, then
`CLAUDE.md`.

Implement EXACTLY what the WORKMAP's UNFINISHED section prescribes, building on
the design-faithful path the validation identified (the oid-traversal union
encoder — Path A: `oidenc`/`lanestore`/`variants`/`reslot`), NOT on a preferred
rewrite.

Deliver: the union/lane encoding persisted as REVERSE-delta VBF frames through
the existing `Depot` (prepend / seal_f1 / cold; f0 = current tip, history
backward, O(1) tip), fed from REAL git via the persistent `cat-file --batch` +
oid tree cache (O(changed) traversal), so any historical ref's tree is served
SHA-exact from STORED bytes.

Obey every hard rule in the shared context. In particular:
- Build on the design-faithful code. Do NOT delete it, do NOT replace it with a
  personal reimplementation. If the workmap flagged a genuine CONTRADICTION, fix
  only that, minimally.
- REVERSE deltas (f0 = tip). PREPEND. On-demand oid tree traversal with the oid
  cache; never materialize whole trees; never hash stored content to reslot.
- Reuse `depot-vbf`/`store`/`depot::stream`. No parallel engine. Minimal diff.

Build until clean (warnings acceptable):
`cd /home/user/sarun/gimir && uv run --with cargo-zigbuild --with ziglang cargo zigbuild --release -p gitdepot --tests --target x86_64-unknown-linux-musl`
Iterate on compile errors yourself. Do NOT commit.

Update `gitdepot/IMPL-NOTES.md`: files changed, the reverse-delta + oid-traversal
data flow, how real git feeds it, and any deviation from the workmap WITH
justification (and any genuine blocker stated plainly). Return a concise summary:
files touched, the wiring in 6 lines, and the final build status line.
