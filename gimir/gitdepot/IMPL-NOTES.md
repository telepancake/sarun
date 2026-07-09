# gitdepot IMPL-NOTES (Phase 6 — Unify + fix the correctness gap)

Worked against `DESIGN.md` (authoritative), `WORKMAP.md`, `EVALUATION.md`.

## Build status

Green with the prescribed command, zero warnings:

    cd /home/user/sarun/gimir && uv run --with cargo-zigbuild --with ziglang \
      cargo zigbuild --release -p gitdepot --tests \
      --target x86_64-unknown-linux-musl
    -> Finished `release` profile

All gitdepot lib unit tests (55) pass; every integration binary
(`roundtrip` 20, `equivalence` 3, `two_frames`, `readout` 4, `spill`,
`staging_size`, `stub` 3, `lanestore*`) passes. No existing test regressed.

---

## 1. The git.git correctness bug — ROOT-CAUSED and FIXED (verified)

`EVALUATION.md` §1: git.git's earliest 98 commits reconstruct to non-existent
tree oids. Reproduced exactly (98/448 on a self-contained slice branched at
`ad87de7c`, git.git's first ~10 days), then diagnosed to the byte.

**Root cause — git normalizes file modes, silently.** git.git's earliest trees
store many blobs with mode **`100664`** (group-writable), which is part of the
exact tree *bytes* and therefore of the tree oid. But `git diff-tree --raw` and
`git ls-tree` — the plumbing the importer read modes from — **normalize
`100664` → `100644`** in their text output. So the importer stored `100644`,
and the reconstructed tree hashed to the wrong (non-existent) oid. Proof: the
raw tree object `git cat-file tree 0776ebe1` shows `100664` on 14 of 20 entries;
`diff-tree --raw` reports all as `100644`.

There are **two** ways the normalization leaks, and both had to be closed:

- **(a) Hidden true mode.** A file first appears (or is content-modified) with a
  non-canonical mode. `diff-tree` reports the normalized mode. — Fixed by
  sourcing the exact mode from the raw tree object: `lib.rs:parse_tree_obj`
  parses a tree body verbatim; `lib.rs:exact_mode` walks raw tree objects along
  a changed path (memoized by oid, O(changed)) to get the mode-faithful bytes;
  `delta_layer` now takes the commit's `tree_oid` and uses `exact_mode` for
  every non-deletion leaf instead of `RawChange::new_mode`.

- **(b) Hidden mode-only change.** A commit that flips a file `100664 → 100644`
  with **no content change** produces **no `diff-tree` entry at all** (both sides
  normalize to `100644`), so the stale inherited `100664` would leak into a tree
  that must reconstruct as `100644`. This is why the naive fix for (a) alone made
  it *worse* (230/448). — Fixed by `lib.rs:mode_only_changes`: a lockstep walk of
  the first parent's raw tree against the child's, **pruned by subtree oid**
  (identical subtree oid ⇒ every mode underneath is already correct, skip), which
  emits synthetic `M` changes for files with equal blob oid but differing mode.
  Wired in `frontier_walk` (append to the commit's change list before
  `delta_layer`).

The shipped reconstruction (`lib.rs:view_tree_oid`) already serialized the raw
`mode` attr bytes verbatim, so once the attr carries `100664` the tree hashes
exactly — no readout change was needed.

**Verification.** Added `examples/verify.rs` (mode-faithful probe: reconstructs
each commit's tree from stored bytes and hashes via the newly-exposed
`lib.rs:git_tree_oid_of_view`, which does **not** go through `layer::Mode`).
Results vs `git rev-parse <sha>^{tree}`:

- git.git first-10-days slice (448 commits, all 98 historical-mode trees + the
  `100664→100644` normalization commit `22b7810`): **0 mismatches** (was 98).
- git.git merge-heavy 2803-commit slice (max frontier 37): **0 mismatches**.
- ripgrep (2278 commits): **0 mismatches** (no regression on canonical-mode
  repos).

Note `examples/treecheck.rs` (the pre-existing probe) hashes through
`layer::tree_oid_of_entries`/`layer::Mode` and therefore *panics* on a `100664`
attr — that is not a store defect but the §2-encoder limitation in §2 below; use
`examples/verify.rs` on any corpus with historical modes.

### Files changed for the fix
- `gitdepot/src/lib.rs`: `parse_tree_obj`, `exact_mode`, `mode_only_changes`
  (new); `delta_layer` gains `new_tree_oid`; `frontier_walk` computes the parent
  tree oid and appends mode-only changes; `git_tree_oid_of_view` exposed `pub`.
- `gitdepot/examples/verify.rs` (new, mode-faithful validation probe).

---

## 2. The same bug is latent in the §2 union encoder — the unification blocker

`layer.rs:Mode` is a four-case `Copy` enum (`File`/`Exec`/`Symlink`/`Gitlink`,
i.e. exactly `100644`/`100755`/`120000`/`160000`) and `Mode::from_octal` returns
`None` for anything else. The whole §2 encoding is built on it: the mode-tag
child is `x`/`l`/`m` (three canonical modes), `VarId = (Mode, Oid)` keys the
reslot, and `tree_oid_of_entries` serializes `Mode::octal()`. **The §2 encoder
as implemented cannot represent `100664` and therefore cannot round-trip git.git
SHA-exactly** — if the union were wired as the TREES payload today it would
reproduce the very bug just fixed, in a place where the readout *does* go through
`Mode`.

This is a genuine, newly-surfaced prerequisite for the unification: before the
§2 union tree can be the SHA-exact TREES payload, `layer::Mode` must carry
arbitrary git mode bytes (e.g. an `Other(Vec<u8>)` variant stored as a mode-tag
child whose blob is the raw mode octal, rather than an empty tag). That change
is not local: `Mode` is `Copy` and referenced at ~130 sites in `layer.rs` plus
`unionstore.rs`, `frame.rs`, `gitsrc.rs`, `shards.rs`, `reflog.rs`,
`geostack.rs`, and the golden-encoding unit tests. It is a bounded but
cross-cutting refactor of the authoritative encoder, and it gates SHA-exactness
of any union-based TREES payload.

---

## 3. Unification: state, and the concrete obstacle to a one-pass landing

Target (task + WORKMAP): one engine that is **both §2-encoding-correct and
§7-direction-correct**, persisted through `store.rs`'s frame machinery as the
TREES payload, reachable from the CLI; then delete `variants.rs`,
`unionstore.rs`, `frame.rs`, `gitsrc.rs` once subsumed.

What exists (confirmed by reading the tree, not assumed):

- `layer.rs` is §2-correct and now has the oid-addressed core
  (`encode_union_oid`, `delta_multi_lane_stacked_oid`, `extract_lane_entries`,
  `reconstruct_lane_tree_oid`) plus `lanes.rs` (`assign_lanes`/`compact_lanes`).
  So the §2 encoder, reverse-capable delta, per-lane reconstruction, and lane
  assignment all already exist as building blocks.
- The two persisted union engines are each half-right and neither is the design:
  **B** (`unionstore.rs` over `layer.rs`) is §2-correct but stores **forward**
  deltas (reverse-at-seal is DEFERRED in its own module doc); **C**
  (`lanestore.rs`/`oidenc.rs` over `variants.rs`) is §7-reverse-correct and
  git-driven and reconstructs per-lane SHA-exact, but its on-disk shape
  (`variants.rs`: nested `\0v/\0m` wrapper, mode-as-attr, bitmap never omitted)
  contradicts §2.

**Why it did not land as one compiling, test-green pass this session — the
concrete obstacle.** The unification is not additive; it is a rewrite of the two
largest, most-entangled files (`store.rs` 2328 lines, `lib.rs` 3012 lines) across
one hard structural seam, gated by the §2 blocker in §2 above:

1. **Index model change (store.rs).** The TREES chain is **commit-indexed**: one
   git tree per commit, a `tree_idx` per commit, readout by `tree_idx`
   (`tree_idx_for`, `flush_tree_batch`, `walk_tree_views`, `tree_view`,
   `staged_views`, and `readout.rs`). The design is **revision-indexed**: each
   record is the *union of all live lanes* at a revision, a commit resolves to
   `(revision, lane)`, and readout reconstructs the revision's union then
   `extract_lane_entries(lane)`. Converting the chain payload also rewrites the
   commit→tree resolution, the `meta.sqlite` ref→tree mapping, and `export`.
2. **Lane-driven ingest (lib.rs).** `frontier_walk` produces one resolved
   `depot::View` per commit and hands it to `add_commit`. The union path needs
   `lanes::assign_lanes` over the whole in-scope DAG, then per-revision live-lane
   sets (each lane's `(mode, oid)` tree) fed to `encode_union_oid` /
   `delta_multi_lane_stacked_oid`. That restructures the ingest sink, not just
   swaps a call.
3. **Direction reconciliation.** Whichever engine is kept must be flipped to the
   design's newest-full/older-reverse: either give `unionstore.rs` the reverse
   lifecycle (`lanestore.rs:seal_prepend` already implements exactly this
   discipline) or retarget `oidenc.rs`'s delta engine — which is built around
   `variants.rs`'s two-key nested shape + `reslot.rs` — onto `layer.rs`'s
   single-node `file_key` shape.

Each of 1–3 is destructive to a currently-green, CLI-reachable path
(`store.rs`/`lib.rs`) and to the off-path union tests; there is no intermediate
state where the tree both compiles and keeps the suite green until all of 1–3
plus the `Mode` extension land together. Per the task's own rule ("remove a file
only once its role is fully taken over and the tests still pass — never before"),
none of `variants.rs`/`unionstore.rs`/`frame.rs`/`gitsrc.rs` could be safely
deleted yet: nothing has taken over their role, so deleting them would only break
the build. They are therefore left in place, not because the convergence is
declined, but because the unified path they would be subsumed by is the
multi-file rewrite described above, gated by the §2 `Mode` blocker.

### Precise remaining plan (in dependency order)
1. Extend `layer::Mode` to carry arbitrary git mode bytes (§2 above) so the
   union tree is SHA-exact-capable; re-green the `layer`/`lanestore` unit tests.
2. Pick the reverse §2 engine: flip `unionstore.rs` to
   newest-full/older-reverse using `lanestore.rs:seal_prepend`'s discipline (this
   is the smaller change — `unionstore` is already §2/`layer.rs`-based), giving
   the one §2+§7 engine. `variants.rs`+`oidenc.rs`+`lanestore.rs`+`reslot.rs`
   then become subsumable; `frame.rs` (B's in-RAM lifecycle) and `gitsrc.rs`
   (toy source) become dead.
3. In `lib.rs`, assign lanes over the DAG and feed per-revision lane-sets to the
   union engine; in `store.rs`, make the TREES record the per-revision union and
   resolve commit→`(revision, lane)`; reconstruct via `extract_lane_entries`.
4. Delete the subsumed files once the suite is green through the union path.

The git.git fix in §1 is independent of all of this and is complete and verified
on the shipping path.
