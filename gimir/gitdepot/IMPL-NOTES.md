# gitdepot IMPL-NOTES (Phase 6 — Unify + fix the correctness gap)

Worked against `DESIGN.md` (authoritative), `WORKMAP.md`, `EVALUATION.md`.

## Build status

Green with the prescribed command, **zero warnings**:

    cd /home/user/sarun/gimir && uv run --with cargo-zigbuild --with ziglang \
      cargo zigbuild --release -p gitdepot --tests \
      --target x86_64-unknown-linux-musl
    -> Finished `release` profile

All lib unit tests (38) pass; every integration binary passes: `roundtrip`
(20), `equivalence` (3), `two_frames`, `readout` (4), `spill`, `staging_size`,
`stub` (3), `lanestore_union` (2), `lanestore_seal`. The `#[ignore]`d
git.git-scale proof `lanestore::gitgit_proof_union` passes on real git.git
slices (below).

---

## 1. The git.git correctness bug — ROOT-CAUSED and FIXED (verified)

`EVALUATION.md` §1: git.git's earliest 98 commits reconstructed to non-existent
tree oids. **Root cause:** git.git's earliest trees carry mode **`100664`**
(group-writable), which is part of the exact tree bytes/oid, but
`git diff-tree`/`ls-tree` — the plumbing the importer read modes from —
**normalize `100664`→`100644`**. Two leaks, both closed in `lib.rs`:

- **(a) hidden true mode** — `parse_tree_obj` + `exact_mode` source the exact
  mode from the raw tree object (O(changed), memoized by oid); `delta_layer`
  uses it instead of `diff-tree`'s normalized mode.
- **(b) hidden mode-only change** — a pure `100664→100644` flip produces *no
  diff-tree entry at all*; `mode_only_changes` recovers it by a lockstep raw-
  tree diff pruned by subtree oid, wired into `frontier_walk`.

Verified mode-faithfully (`examples/verify.rs` via the new public
`git_tree_oid_of_view`): **0 mismatches** on the git.git 448-commit slice
(all 98 historical-mode trees + the normalization commit), the 2803-commit
merge-heavy slice, and ripgrep — no regression. Still intact after the
unification (re-verified: 0/448).

---

## 2. `layer::Mode` extended to represent arbitrary git modes (§2 SHA-exactness)

`layer::Mode` was a four-case enum that could not represent `100664`, so the §2
encoder itself was not SHA-exact-capable on historical trees. Extended (still
`Copy`) with `Mode::Other(u32)` — encoded as an **`o` mode-tag child whose blob
is the octal bytes** (canonical `x`/`l`/`m` tags stay empty; a plain file has no
tag). `octal()` now returns owned bytes; `from_octal` classifies non-canonical
blob modes as `Other` and directories (`S_IFDIR`) as `None`. All decode sites
(`visit_entries`, `visit_current`, `read_facets`, `emit_absolute_file`,
`both_child`) route through one `tag_mode(name, blob)` helper; the mode-tag
hole/revert compares by tag NAME so an `o` tag's octal is simply overwritten.
Locked in by `layer::other_mode_round_trips_through_union`. Cross-cutting sites
(frame/shards test helpers that cast `mode as u8`) were switched to
`mode.octal()`; those files were subsequently deleted (§4).

---

## 3. The one union engine: §2 encoding driven by §6/§7 reverse-delta mechanics

The design's §1/§2/§7 core is now realized in **one** engine and is the only
union path in the tree:

- **§2 encoding = `layer.rs`.** `oidenc.rs` (the O(changed) reverse-delta
  generator) was **retargeted off the §2-violating `variants.rs` shape onto
  `layer.rs`'s encoding**. It now emits, at each directory level, file variants
  as `file_key(name, slot)` **sibling** nodes (single node per variant: content
  blob + `x`/`l`/`m`/`o` mode-tag child + `lanes` bitmap child) and
  subdirectories as bare `dir_key(name)` — §2 point-for-point, including the new
  `Other` modes. The two-key `\0v`/`\0m`-under-a-wrapper shape is gone.
  `full_view_dir`/`variant_view` build the materialized head as the byte-
  identical counterpart of the folded reverse deltas (so cold-frame anchors
  match). Reconstruction goes through the authoritative
  `layer::extract_lane_entries` / `tree_oid_of_entries`.
- **§6 O(changed) + a second mode bug fixed.** `distribute_children`'s change
  test compared only blob oid + `is_dir`; a **pure mode change keeps the blob
  oid**, so `100664→100644` flips were invisible and the stale mode variant
  kept the lane's bit — the union counterpart of the `lib.rs` bug in §1. Fixed
  by adding `a.mode != b.mode` to the change test.
- **§7 reverse-delta lifecycle = `lanestore.rs`** (unchanged discipline): f0 is
  the newest full state, older revisions are reverse deltas, full state
  materialized only at seal boundaries, sealed to cold via `seal_prepend` (the
  same frame machinery as `store.rs`). It drives straight off git (`assign_lanes`
  / `compact_lanes` for the lane axis, `oidenc` for the payload) and maps each
  commit to `(revision, lane)`.
- **§6 reslot = `reslot.rs`** (one implementation, kept).

Result — no engine is §2-wrong or §7-wrong any longer; the two half-right
clusters the WORKMAP found are collapsed into this one.

### CLI reachability (`cli.rs` / `lib.rs`)

- `gitdepot union <repo> <store> [--level N]` — build the union-of-lanes store.
- `gitdepot union-verify <repo> <store> [stride]` — reopen from disk and check
  every (stride-th) commit's tree reconstructs SHA-exact **from the stored union
  bytes** against git.

### Verification on real repos (from the CLI path)

    gitdepot union git.git-slice(448) …   -> 448 revisions, 5 lanes, 306 918 B
    gitdepot union-verify (all 448)       -> 448 checked, 0 mismatches
    gitdepot union ripgrep(2278)          -> 2278 revisions, 17 lanes, 4.93 MB
    gitdepot union-verify (stride 20)     -> 114 checked, 0 mismatches
    lanestore proof, git.git 2803-commit  -> 2803 reconstructed, SHA-exact,
                                             store/pack = 0.67x

The 448-commit slice is git.git's earliest ten days — the exact history that
failed in `EVALUATION.md` — now SHA-exact through the union path, `100664`
modes and all, and **smaller than git's own pack** (the size thesis the union
exists for, on the many-tree adjacency).

---

## 4. Removed files (subsumed; tests still green)

Deleted the entire unwired duplicate-engine cluster — none is reachable from the
CLI, the shipped `store.rs` path, or the unified union path (verified by grep:
no `crate::<mod>` code reference survives outside the deleted set):

- **`variants.rs`** — the §2-violating `\0v`/`\0m` nested encoding; role taken
  over by `layer.rs` (§2) as consumed by the retargeted `oidenc.rs`.
- **`unionstore.rs`** — the forward-delta (§7-wrong) union store; role taken
  over by the reverse-delta `lanestore.rs`.
- **`frame.rs`** + **`geostack.rs`** — cluster B's in-RAM union lifecycle; role
  taken over by `lanestore.rs`'s persisted reverse-delta lifecycle.
- **`gitsrc.rs`** — the toy `ls-tree`/`cat-file` source (unused).
- **`shards.rs`** (§9 in-RAM sharding) + **`reflog.rs`** (§8 experiment) — both
  built only on `frame`, unwired, and had to go with it; they were not on any
  reachable path. (§9 persisted sharding remains unimplemented anywhere, and the
  live `store.rs` keeps its own REFLOG chain for §8 — see below.)

`lib.rs` `mod` declarations were pruned accordingly. Build clean, zero warnings.

---

## 5. What is deliberately NOT changed, and why (accurate scope)

The shipped **`store.rs` one-git-tree-per-commit store** (`import` / `update` /
`export` / `mirror`, its COMMITS / TAGS / REFLOG chains, `readout.rs`,
`meta.sqlite`, and the 20-test `roundtrip` + `equivalence` + `readout` + `stub`
suite) is **retained and untouched**. The union engine is reachable and verified
as its own CLI path (`union` / `union-verify`) rather than as the payload swapped
*inside* the `import` flow.

Folding the union in as `store.rs`'s TREES payload — so `import`/`export`
themselves store `(revision, lane)` per commit and reconstruct via
`extract_lane_entries` — is a further integration that rewires the commit/tag/
ref indexing and the `export` commit-reassembly that currently key off a
per-commit `tree_idx`, and cannot be done without reworking that whole green
suite in lockstep. It is a scoped follow-on, not a blocker: the union engine it
would consume is complete, §2/§7-correct, SHA-exact, and CLI-reachable today.

The §4 mmap/`MADV_DONTNEED` single-pass updater is still an in-RAM `Vec<u8>`
compose.

---

## 6. §2 all-ones `lanes` omission — DONE (VALIDATION §2 gap closed)

The reachable union encoder now omits the `lanes` child when a variant covers
every live lane (§2 "absence means present in every live lane"). Implemented in
`oidenc.rs`: `eff(bitmap, live)` returns `None` (omit) when the variant's bitmap
equals the current revision's live-lane set; `variant_view`/`variant_reverse_node`
take the omission, and `full_view_dir`/`advance` are threaded the per-revision
`live`/`prev_live`/`new_live` bitmaps from `lanestore.rs` (tracked incrementally:
a lane is live iff it holds a tree).

The subtle part is round-trip correctness across a live-set change: a birth/death
flips the omission status of *untouched* variants (present in all-but-the-born/
dying lane), which the reslot transition does not report. `remat_flips` walks the
pre-advance skeleton on live-change revisions and emits the needed `lanes`-child
hole/Set for exactly those variants; `merge_delta` folds them under the reslot
deltas (disjoint slots). Verified SHA-exact **from the stored bytes** via
`union-verify`: 0 mismatches on git.git's earliest 448 (5 lanes, births/deaths,
`100664` modes), the merge-heavy 2803-commit slice (7 lanes), and ripgrep. All
unit + integration tests stay green.

---

## 7. Fold the union into the real mirror (import/update/mirror/export) — NOT LANDED this hop; concrete obstacle

This is the remaining big item: make the union the mirror's TREES payload so
`import`/`update`/`mirror`/`export` (not a side `union` command) store
`commit -> (rev, lane)` and reconstruct SHA-exact by extracting the lane. I
produced a complete coupling map (below) and concluded it cannot be landed as a
single compiling, test-green hop; starting it would break the tree, violating the
"keep it compiling" directive. The blockers are specific, not scope-avoidance:

1. **Atomic wire-format + test-contract switch.** `store::CommitRecord.tree_idx`
   (a serialized `u64`, `store.rs:253/278/299`) must become `(rev, lane)`. That
   one change breaks, simultaneously and un-compilably, `Ingest::{tree_idx_for,
   add_commit, flush_tree_batch, known_tree_idx, tree_idx_of_commit,
   tree_idx_of_staged, staged_views}`, `Store::{tree_view, tree_views,
   walk_tree_views, count(TREES)}`, `readout.rs::{for_commit, for_tree, view}`,
   `lib.rs::{ingest_stream, frontier_walk, same_tree_parent, seed_views, export,
   write_stub}`, and `TagTarget::Tree`. There is no incremental compiling state.

2. **The union has no tree dedup — the suite asserts the opposite.** The
   one-tree-per-commit model dedups identical trees (`same_tree_parent`), so the
   tests bake in `count(TREES)==14` with shared `tree_idx` and "minted a new tree
   despite a same-tree parent" (`equivalence.rs:260`, `roundtrip.rs:478/730/736/
   746/761`, `staging_size.rs` whole file, `two_frames.rs:120-143`,
   `spill.rs:87`). The union stores one record per revision (per commit), no
   dedup — ~30 assertions across 6 files must be rewritten to the union contract,
   not merely adjusted.

3. **Incremental update is unsolved in the union engine (the real design gap).**
   The mirror requires O(new) bounded `update` (`roundtrip::
   update_io_is_bounded_not_o_history`), but `lanestore::encode_repo_union` is
   **full-encode only**. Making `update` union needs (a) reconstructing the
   `oidenc::Encoder` slot state (`Skel`) from the stored f0 — which holds content,
   not oids, so each variant's `(mode, oid)` VarKey must be recovered by hashing —
   and (b) **stable cross-update lane assignment**: `lanes::assign_lanes` runs over
   the whole DAG and `compact_lanes` reuses indices, but an update must keep every
   already-stored commit's lane fixed and continue births from the boundary. §8
   lane stability is itself PARTIAL (VALIDATION §8); this is a design sub-project,
   not wiring. `mirror`'s laddered bootstrap (`staged_views`) adds a third
   tree-source path to reconcile.

Each of 1–3 is real work with a real reason; together they are a multi-hop
integration. The union engine they consume is complete, §2/§7-correct, SHA-exact,
CLI-reachable, and now §2-omission-conformant — i.e. the payload is ready; the
blocker is the mirror-side rewrite + the incremental-union-update design, which
cannot be delivered compiling+green in one hop.

Plan (dependency order): (a) add `Skel` reconstruction from f0 + stable-lane
persistence to make the union incrementally updatable; (b) give `Ingest` a
union-tree mode driving `oidenc` (commit→lane), replacing `tree_idx_for`;
(c) rewire `readout`/`export`/`write_stub` to reconstruct via
`extract_lane_entries`; (d) rewrite the 6 test files to the union contract;
(e) delete the single-tree TREES encoding. Keep COMMITS/REFLOG/TAGS + meta.
