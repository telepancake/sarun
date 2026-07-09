# Phase 6 — Unify onto the design, then remove what it replaces

Read `gitdepot/pipeline/00-context.md`, `gitdepot/DESIGN.md`,
`gitdepot/WORKMAP.md`, and `gitdepot/IMPL-NOTES.md`.

Today the design lives in three separate places (WORKMAP): the §2 encoding
(`layer.rs`), the §6/§7 mechanics — oid-on-demand traversal, reverse-delta-from-
newest, O(changed) diff (`oidenc.rs`/`lanestore.rs`) — and the §10/§11
persistence + real-git ingest (`store.rs`/`lib.rs`). No single path is the whole
design, and the CLI reaches only the plain one-tree-per-commit store.

Make one path that is the design: the §2 encoding (`layer.rs`) driven by the
§6/§7 oid-traversal/reverse-delta mechanics, persisted through `store.rs`'s
existing frame machinery as the TREES payload, reachable from the CLI. Build on
the code the WORKMAP found to conform; port mechanics onto the conforming
encoding rather than re-deriving them.

Remove a file only once its role is fully taken over by the unified path and the
tests still pass — never before. (Candidates the WORKMAP names: `variants.rs`,
`unionstore.rs`, `frame.rs`, `gitsrc.rs`. Confirm each is truly subsumed before
deleting it.)

Also fix the correctness gap the evaluation found: git.git's earliest commits
reconstruct to wrong tree oids (`EVALUATION.md`). Find the cause and fix it, or,
if it is genuinely separable, describe it precisely in `IMPL-NOTES.md`.

The deliverable is the whole unification working and reachable from the CLI —
not a first step toward it. "Large and interdependent" is the task, not a reason
to stop; do the interdependent work. If you truly cannot finish a part, that is a
specific technical obstacle you hit and must state concretely (what you tried,
why it blocks) in `gitdepot/IMPL-NOTES.md` — not a scope you chose to defer to
someone later.

Build until it compiles. Write what you changed, what you removed and why it was
safe, and any genuine remaining obstacle, to `gitdepot/IMPL-NOTES.md`. Return a
short summary and the build status.
