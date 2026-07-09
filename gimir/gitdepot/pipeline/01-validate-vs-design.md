# Agent 1 — Validate existing code against the recovered design

Read `gitdepot/pipeline/00-shared-context.md` first, then `gitdepot/DESIGN-RECOVERED.md`
in full. Cross-check the transcript (user turns) for any unclear point.

Your job is VALIDATION, not preference. For every relevant source file, decide —
with `file:symbol` evidence AND a citation to the design section — whether it
IMPLEMENTS, PARTIALLY implements, or CONTRADICTS the design.

Read and assess at minimum:
- Path A: `gitdepot/src/{oidenc.rs, lanestore.rs, reslot.rs, variants.rs}`
- Path B: `gitdepot/src/{layer.rs, gitsrc.rs, frame.rs, unionstore.rs}`
- `gitdepot/src/{store.rs, lib.rs}` — and how the SHIPPING ingest reads git
  (`rev-list`, `diff-tree --stdin`, `cat-file --batch`), and whether it uses the
  union/lane encoding or plain per-tree trees.
- `depot/src/stream.rs`, `depot-vbf/`, and the `Depot` seam
  (prepend / seal_f1 / stream_frame_records / cold_iter).

Rules for classification:
- "Delete" is allowed ONLY for genuine CONTRADICTIONS of the design (cite the
  exact rule). Unwired-but-design-faithful code is KEEP + WIRE, never delete.
- Judge Path A vs Path B strictly by design-conformance, per section:
  - git-tree traversal: on-demand by oid + oid cache + O(changed) pruning
    (Path A) vs whole-tree `ls-tree` into flat maps (Path B).
  - delta direction: REVERSE (design) vs FORWARD (`unionstore.rs`).
  - reslot: slot algebra by (mode,oid) incl. bitmap-similarity (Path A) vs
    exact-oid-only (`layer.rs`).

Write `gitdepot/WORKMAP.md` with, each entry `file:symbol` + design-section
citation + one-line reason:
1. IMPLEMENTS DESIGN (keep): design-faithful code, wired or not.
2. CONTRADICTS DESIGN (fix or remove): only genuine contradictions, each with
   the exact design rule it violates.
3. UNFINISHED (the wiring): precisely what must connect so the DESIGN-faithful
   traversal/union encoder persists via REVERSE-delta VBF frames through the
   existing `Depot`, fed by real git (`cat-file --batch` + oid tree cache,
   O(changed)), and serves any historical ref SHA-exact from stored bytes. Name
   the producer and consumer integration points in `store.rs`/`lib.rs`, reusing
   the existing frame codec.
4. TERMINOLOGY MAP aligning encoder vocabulary with depot-vbf/store (f0/f1,
   PREPEND, seal_f1, cold).
5. RECONCILE A vs B: per design section, state which implementation is
   design-faithful and should be built on, and which diverges — with evidence,
   not preference. Do NOT recommend deleting Path B wholesale; state only what
   the design requires.

Do NOT modify any source file. Do NOT commit. Output is `WORKMAP.md` plus a
15-line summary returned as your final message: the A-vs-B verdict, the top
contradictions (if any), and the exact first wiring step.
