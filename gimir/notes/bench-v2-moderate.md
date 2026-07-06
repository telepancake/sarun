# gitdepot v2 sanity bench — moderate repo (2026-07-06)

Fixture: synthetic, 500 commits / 800 files / 23MB tree / tags+branch
(the workspace repo is a SHALLOW clone — v2 refuses shallow parents
loudly where v1 stored dangling shas; use full-history sources).
Baseline: fresh `repack -adf` pack = 12.0MB. Tool:
`cargo run --release -p gitdepot --example bench -- <store> [deep_sha]`.

| op | result |
|---|---|
| full mirror (500 commits) | 2m12s (~0.26s/commit) |
| no-op mirror | 0.11s |
| 1-commit update | 0.62s (bounded ✓) |
| resolve_ref | 29ms |
| tip decode + root ls | 70ms |
| tip full 800-file walk | 0.5ms |
| tree 400 deep | 32s ✗ |
| store vs pack | 47.5MB vs 12.0MB (3.96×) ✗ |

Diagnosis:
- Size: cold tier = 13.0MB ≈ pack (encoding at pack parity ✓); the
  rest is DEAD BYTES in f0 (32MB file, superseded head frames) + f1 —
  reclaim happens only on 32MiB file roll. Fix: smaller depot file
  threshold for gitdepot (e.g. 4MiB) shrinks the garbage window ~8×.
- Deep access: cached walk costs ~80ms/step × depth; dominated by the
  full-view RE-ENCODE per frame boundary (refPrefix) + full-view
  apply per step = O(depth × tree). Design fork, user to decide:
  (1) delta-anchored frames (prefix = previous delta record, already
      in hand): kills the re-encode; with in-place apply → O(depth ×
      delta). Slightly worse compression than view-anchoring.
  (2) checkpoint records (full layer every K): keeps view-anchoring
      compression, bounds walk to K steps, +1 full record per K.
  They compose: checkpoints + delta-anchoring between them is the
  likely end state (K trades size vs worst-case access).

## 2026-07-06 — in-place walk (apply_mut)

Fix landed: `depot::apply_mut` (in-place O(delta) apply, property-tested
equal to `apply`) + `Store::walk_tree_views` keeps ONE working view
mutated per record instead of a fresh full `apply` + Vec of every
intermediate view. Anchoring discipline and on-disk format unchanged
(the full-view re-encode remains once per cold frame; byte-fidelity
pinned by `tree_walk_matches_apply_reference_byte_exact`).

Fixture regenerated per the recipe above (seed 7, 500 commits / 800
files / 23.4MB tree; pack 12.8MiB; store 28M — the 4MiB depot file
threshold is already in). Same store, same deep sha (main~400):

| op | before | after |
|---|---|---|
| resolve_ref | 8.3ms | 9.8ms |
| tip decode + root ls | 67ms | 80ms |
| tip full 800-file walk | 0.46ms | 0.49ms |
| tree 400 deep | 16.3s | **0.45s** ✓ |

(16.3s not 32s pre-fix here: this store was written with the 4MiB file
threshold, so its cold tier differs from the original 2026-07-06 run;
same pathology, same O(depth × tree) shape.) Deep access is now ~1ms/step
+ one full-view re-encode per cold frame — well under the 2s target.
