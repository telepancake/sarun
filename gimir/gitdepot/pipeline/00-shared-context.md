# Shared context for all pipeline agents

## Authority
- The USER's design is authoritative. It is recorded in `gitdepot/DESIGN-RECOVERED.md`.
- If any point is unclear, cross-check the USER's own words in the transcript at
  `/root/.claude/projects/-home-user-sarun/5df6fa05-fb5d-5959-9227-2afc1158ad07.jsonl`
  (human/user turns only — NOT tool results, NOT assistant turns).
- Do NOT substitute your own judgment or preference for the design. You validate
  and implement the design; you do not redesign it.

## Hard rules (the user's explicit, repeatedly-violated instructions)
1. Do NOT delete design-faithful code. Unwired ≠ dead. Only genuine
   CONTRADICTIONS of the design may be removed (e.g. the one part the user
   explicitly rejected: `Skel` full-materialization), and only with the exact
   design rule cited.
2. git tree traversal is BY OID ON DEMAND with a parsed-tree cache keyed by oid;
   unchanged subtrees are pruned by oid (O(changed) per commit). NEVER
   materialize whole trees into flat maps. NEVER hash stored content to reslot.
3. Storage is REVERSE-delta: f0 = the CURRENT tip refPrefix, history stored as
   reverse deltas, tip read is O(1). NOT forward deltas.
4. newest-first storage means PREPEND, never "append".
5. Reuse the existing machinery (`depot`, `depot-vbf`, `store.rs`,
   `depot::stream`, the `Depot` prepend/seal_f1/cold path). No parallel/pet
   engine. Minimal diff.
6. No invented framings. Do NOT declare the design broken. If genuinely blocked,
   write the blocker plainly in `gitdepot/IMPL-NOTES.md` and implement the best
   design-faithful version.

## Build / test commands
- Build (static musl, the only build): 
  `cd /home/user/sarun/gimir && uv run --with cargo-zigbuild --with ziglang cargo zigbuild --release -p gitdepot --tests --target x86_64-unknown-linux-musl`
- Lib test binary: `/home/user/sarun/gimir/target/x86_64-unknown-linux-musl/release/deps/gitdepot-*`
  (pick the one whose `--list` shows lib tests). Integration test bins are the
  other `gitdepot-*` binaries built from `tests/`.

## The two parallel implementations you must judge strictly against the design
- Path A (design-faithful git-tree traversal): `oidenc.rs` (`Objects::tree` —
  read tree objects by oid on demand, prune unchanged subtrees by oid, cache
  parsed trees by oid), `lanestore.rs` (persistent `cat-file --batch` adapter +
  bounded oid→tree cache; per-revision state = union of live lanes' git trees,
  diffed by oid, O(changed)), `reslot.rs` (slot algebra incl. bitmap-similarity
  matching), `variants.rs` (union-variant on-disk representation + lane reader).
- Path B (the assistant's second path): `layer.rs`, `gitsrc.rs` (reads whole
  trees via `git ls-tree` into flat maps — no oid cache, no O(changed) pruning),
  `frame.rs`, `unionstore.rs` (stores FORWARD deltas).
- Judge each against the design by section and evidence, not by preference.
