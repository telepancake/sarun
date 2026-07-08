# gitdepot WORK MAP

Grounded in DESIGN-RECOVERED.md (authoritative) + ASSEMBLY.md + the code on
disk. The repo currently holds **two parallel implementations**:

- **Path A — LIVE & PERSISTED (one-tree-per-record).** `lib.rs` +
  `store.rs` + `wikimak_depot::Depot` (`depot-vbf`). Drives real git
  (import/update/mirror/export), stores each git tree as a `depot::View`
  reverse-delta record in the TREES chain, SHA-exact, tested against real
  git.git. This is NOT the union-of-lanes design — it is the working
  scaffold whose TREES-chain payload must be replaced.
- **Path B — DESIGN-FAITHFUL PRIMITIVES, NOT PERSISTED.** `layer.rs`,
  `geostack.rs`, `frame.rs`, `lanes.rs`, `reflog.rs`, `shards.rs`,
  `gitsrc.rs`. The union/variant encoding, reslot-by-oid, geometric stack,
  lanes, in-memory frame lifecycle. SHA-exact against a git-oid oracle but
  lives entirely in RAM — never reaches `depot-vbf`/`store`.
- **Dead skeleton cluster — DELETE.** `lanestore.rs`, `oidenc.rs`,
  `reslot.rs`, `variants.rs`. The rejected "skeleton"/materialize approach.
  Self-contained, referenced by nobody outside itself.

The real work is joining B's encoding into A's persistence and deleting the
dead cluster.

---

## 1. DONE & CORRECT (keep)

Faithful to the recovered design, SHA-exact tested.

- `layer.rs:encode_union` / `variant_node` / `file_key` / `dir_key` — union
  tree shape (§2): `name\0<slot>` variants, bare dirs, `x/l/m` mode-tag
  children, `lanes` bitmap child omitted when all-ones. Correct.
- `layer.rs:container_cmp` / `entry_cmp` — the two orders (§3): authoritative
  container order for the big side, git `base_name_compare` only for
  reconstruction. Names stay clean.
- `layer.rs:delta_multi_lane` / `delta_multi_lane_stacked` — reslot by
  `(mode,oid)` read for free from lane trees, never hashing stored content
  (§5.2). Correct.
- `layer.rs:visit_current` — lockstep read of `refPrefix + stack` with no
  materialized union (§5.1, §10). This is the pillar; keep.
- `layer.rs:visit_entries` / `walk` / `reconstruct_lane_tree_oid` /
  `extract_lane_entries` — single-pass iterator + SHA reconstruction.
- `geostack.rs:GeoStack::push` / `collapse` — geometric 70%-rule stack (§7).
  Model-tested, codec-independent. Correct.
- `depot::stream::compose_stream` — delta∘delta, holes survive (§4). The
  merge; tested.
- `depot::stream::overlay_full` — delta∘full, holes dissolve (§4). The apply.
- `depot::stream::diff_stream` / `diff_stream_holes` — byte-level delta
  generators (§4).
- `lanes.rs:assign_lanes` — persistent ancestry-frozen lane assignment,
  first-parent inheritance, compaction-with-reuse (§6). Correct.
- `reflog.rs` — per-layer lanes+refs, `#lanes ≥ #refs`, DAG capstone that
  drives lanes→union→git oid (§5, §6). Keep as the validated capstone.
- `shards.rs` — per-shard threads, stable full-path hash, cross-shard
  reconstruction, oid invariant across shard counts, lockstep advance (§9).
- `gitsrc.rs:read_commit_tree` — live git blob source (`ls-tree`+`cat-file`)
  feeding `LaneTree`s (§11). The integration seam for a real repo.
- `frame.rs:Frame` (advance/union/seal/reconstruct_tree_oid) — the correct
  **shape** of the §7 lifecycle (refPrefix + geostack + seal, no skeleton),
  SHA-exact tested. KEEP AS THE INTEGRATION TEMPLATE — but see §3: its
  persistence is a stub (in-memory, seal discards history).
- `store.rs` Depot machinery — `open_depot`, four-chain layout
  (TREES/COMMITS/REFLOG/TAGS), `stream_frame_records`, `encode_batch`,
  stable oldest-first indices, `seal_threshold`/`seal_f1`, `Ingest`
  bookkeeping, `meta.sqlite` current-refs-only. §8 persistence is REAL and
  correct; reuse it wholesale.
- `lib.rs` git plumbing — `LogStream`, `CatFile`, `dag_scope`, `walk_order`,
  `Frontier`, ls-remote/fetch/mirror/export. The real-git ingest/serve
  scaffold; reuse.

## 2. DELETE IMMEDIATELY

Dead skeleton reimplementation — the exact "materialize a per-path skeleton
of malloc'd nodes" approach the user rejected (§12.2). Referenced only within
its own cluster; nothing in the live path or Path B touches it.

- `oidenc.rs` — **`struct Skel`**, the per-path variant skeleton. The
  rejected approach. Used only by `lanestore.rs`.
- `lanestore.rs` — skeleton-driven lane encoder; `GITDEPOT_SKEL_MEASURE` /
  `advance_skel`. Used by NOBODY (only a mod decl + a stale doc mention in
  `lib.rs:1355`). Superseded by `layer.rs` + `frame.rs`.
- `reslot.rs` — `Slots`/`Occupant`/`Bitmap` reslotter. Used ONLY by
  `oidenc.rs`. Superseded by `layer.rs:delta_multi_lane` (reslot-by-oid).
- `variants.rs` — `content_key`/`meta_key`/`leaf_delta`/`extract`. Used ONLY
  by `oidenc.rs`+`lanestore.rs`. `layer.rs` reimplements this cleanly.
- `readout.rs` — declared (`lib.rs:104`), referenced nowhere else. Dead.

Also fix the misleading claim: `lib.rs:1355` doc says ingest is driven by
"the lane-store encoder (`lanestore`)" — false; nothing drives it. Remove the
mod decls (`lib.rs:99–104`) and the doc mention when deleting.

Do NOT delete `frame.rs` — despite its in-memory `seal()` discarding history,
it does not duplicate `wikimak_depot::Depot` (a different layer: the union
geostack driver, not a VBF frame chain). It is the template for §3.

## 3. UNFINISHED — the real work

Wire Path B's union encoding into Path A's persisted Depot so the TREES chain
stores union-of-lanes layers (not one-tree-per-record Views), and any
historical ref's tree serves SHA-exact from stored bytes. `store.rs`'s Depot,
seal/cold, stable indices, sqlite bookkeeping are REUSED unchanged; only the
TREES-chain *payload* and its producer/consumer change.

**Producer seam (write side)** — replace per-tree View deltas with union
layers:
- `lib.rs:tree_layer` / `delta_layer` / `frontier_walk` / `ingest_stream`
  currently build a `depot::View`/`Layer` per commit. Replace with: assign
  commits to lanes (`lanes::assign_lanes`), build `LaneTree`s from
  `gitsrc::read_commit_tree`, generate the delta with
  `layer::delta_multi_lane_stacked(refPrefix, stack.layers(), old_lanes,
  new_lanes)`, push onto a `geostack::GeoStack`. This is exactly
  `frame::Frame::advance` — promote that logic out of `frame.rs` into the
  ingest path.
- `store.rs:flush_tree_batch` — currently encodes single-tree reverse deltas
  into TREES. Change it to prepend the geostack's live delta layers (as
  `codec` byte records, oldest→newest) via the existing
  `Depot::prepend`/`stream_frame_records` path. The union bytes ARE
  `depot::codec` bytes, so the frame machinery carries them unchanged.
- Frame-write/seal: on a frame write, flatten the geostack
  (`GeoStack::collapse(compose)`) and `overlay_full` onto the old refPrefix
  to produce the new refPrefix full-state record (== `frame::Frame::seal`),
  then let `store.rs`'s existing `seal_f1`/cold-frame path retire the old f1
  verbatim (§8 seal). refPrefix = the TREES f0 record.

**Consumer seam (read side)** — serve a historical ref's tree from union
bytes:
- `store.rs:tree_view` / `walk_tree_views` / `peeled_tree_oid` and
  `lib.rs:view_tree_oid` / `materialize_tree` currently walk single-tree View
  deltas. Replace tree reconstruction with `layer::visit_current` (lockstep
  `refPrefix` + live stack, no materialization) then
  `layer::reconstruct_lane_tree_oid` / `extract_lane_entries` for the wanted
  lane. A ref → (commit_idx, lane, revision) → union bytes for that revision
  → that lane's git tree.
- Cold/historical revisions: the union layers walk newest→oldest exactly like
  today's reverse-delta walk; `visit_current` over the reconstructed
  refPrefix+deltas at the target revision yields the lane's entries.

**Lane/reflog feed**:
- `lanes::assign_lanes` (upstream of `delta_multi_lane`) supplies the aligned
  `old_lanes`/`new_lanes` arrays; the lane→tree-oid map is reflog-derived
  (`reflog.rs`), the only extra state. One `store.rs` REFLOG batch record per
  written layer records the layer's lanes+refs — reuse `encode_batch`.

**Sharding path** (§9) — `shards.rs` runs per-shard `Frame`s in memory today.
Persisted form: one Depot **per shard** (or one chain-group per shard) each
with its own refPrefix/stack/lanes, advancing in lockstep (empty deltas OK).
Cross-shard tree reconstruction (already in `shards.rs`) gathers each lane's
entries from every shard and hashes together. UNBUILT against `store.rs`.

**Multi-lane variant-locality path** (§6.1, deferred not dropped) — lanes are
stored full side-by-side today (correct first step). Delta-among-lanes
(base-lane pick + reframe) is a later optimization; do NOT build it before
full-lanes-through-Depot is green.

## 4. TERMINOLOGY MAP

Use ONE vocabulary. Left = layer.rs/design; right = store.rs/depot-vbf.

| Path B (union engine) | Path A (Depot/VBF persistence) |
|---|---|
| `refPrefix` (full-state union) | TREES chain **f0** record (`codec` bytes) |
| live delta layer on the geostack | an additional TREES **f1** record, newest-first |
| geostack collapse + `overlay_full` at a frame write | produce the new f0; old f1 → **cold** verbatim |
| `frame::Frame::seal` | `store.rs` frame-write + `Depot::seal_f1` |
| `frame::Frame::advance` | `store.rs:flush_tree_batch` (rewritten) → `Depot::prepend` |
| union bytes | `depot::codec`-encoded `Layer` record |
| delta∘delta (`compose_stream`) | stack compaction between prepends |
| delta∘full (`overlay_full`) | apply-at-seal → new f0 full-state |
| lane / lane tree | REFLOG chain entry (per layer), `#lanes ≥ #refs` |
| lane→tree-oid map | reflog-derived, in-RAM for the ingest |
| shard (path-hash bucket) | one Depot / chain-group per shard |
| "prepend" (NEVER "append") | `Depot::prepend` — newest-first |

Note: `depot-vbf::VbfDepot`/`VbfStore` is a cleaner wrapper over
`wikimak_depot::Depot` used by tests/other mirror crates; `store.rs` calls
`wikimak_depot::Depot` directly with its own frame code. Pick ONE (prefer
extending `store.rs`'s existing path) — do not add a third.

## 5. ORDER OF OPERATIONS (smallest diff, reuse-first)

1. **Delete the dead cluster** (`lanestore.rs`, `oidenc.rs`, `reslot.rs`,
   `variants.rs`, `readout.rs`) + their `lib.rs` mod decls + the stale
   `lib.rs:1355` doc line. Compile clean. Zero behavior change (all unused).
2. **Promote `frame::Frame` logic into a persisted producer.** Keep
   `frame.rs` as the reference; add a `store.rs` path where
   `flush_tree_batch` builds union delta layers via `delta_multi_lane_stacked`
   + `GeoStack` and prepends them through the EXISTING `Depot::prepend` /
   `stream_frame_records` seam. Wire `lanes::assign_lanes` + `gitsrc` in.
3. **Frame-write = seal:** on frame write, `GeoStack::collapse` + `overlay_full`
   → new f0; reuse `Depot::seal_f1`/cold for the old f1. No new frame engine.
4. **Read side:** rewrite `tree_view`/`peeled_tree_oid`/`view_tree_oid` to
   reconstruct via `visit_current` + `reconstruct_lane_tree_oid`. Prove
   SHA-exact on real git.git (import → export round-trip), reusing existing
   `lib.rs` export tests as the oracle.
5. **Single shard first** (shard-bits=0), full lanes, no delta-among-lanes.
   Get import/update/export green end-to-end before touching multi-shard
   persistence (`shards.rs` → per-shard Depot) or variant-locality deltas.

Guiding rule: minimal, direct, data-first. Reuse `store.rs`'s Depot/seal/index
machinery and Path B's tested primitives. Do NOT build a parallel engine — the
dead cluster in §2 is exactly what happens when you do.
