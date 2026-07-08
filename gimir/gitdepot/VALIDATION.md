# gitdepot — Design-Conformance Validation (Agent 5)

Validated `DESIGN-RECOVERED.md` point-by-point against the code on disk
(`src/*.rs`) and the test wiring. Method: read every pillar's implementation
and, critically, trace whether it is **wired into the live git mirror** or is an
**unwired library exercised only by its own `#[cfg(test)]`**.

## The one structural fact that governs everything

The repo contains **two disjoint subsystems** that the design intends to be
**one**:

- **Path A — the shipping mirror.** `lib.rs` (git ingest) → `store.rs`
  (`Ingest`, four VBF chains) → `readout.rs`. This is git-wired and real-git
  tested (roundtrip/readout/equivalence/spill/stub/two_frames). It persists and
  re-serves real history SHA-exact via **reverse-delta VBF**. **But it does NOT
  use the variant/union/lane encoding at all** — `tree_layer` (`lib.rs:339`)
  builds a plain per-commit `depot::Node` tree; `grep encode_union|LaneTree|
  Frame|UnionStore src/store.rs` → **0 hits**.

- **Path B — the design's actual data model.** `layer.rs`, `geostack.rs`,
  `lanes.rs`, `reflog.rs`, `shards.rs`, `frame.rs`, `unionstore.rs`. This is
  where variants, lanes, reslot-by-oid, the geometric stack, and sharding live.
  **Every entry point of Path B (`assign_lanes`, `compact_lanes`, `Reflog`,
  `Frame::seed`, `Shards::seed`, `UnionStore`, `advance`) is called ONLY from
  test modules** — never from the importer, CLI, or `store.rs`. It is an island.

So the design's central encoding is not what the mirror stores, and where that
encoding *is* persisted (`UnionStore`) it is fed **synthetic hand-built lane
trees**, never real git, and stores **forward deltas from an oldest base** — the
opposite of the design's mandated direction.

## Point-by-point

| # | Design pillar | Verdict | Evidence / gap |
|---|---|---|---|
| 1 | **Variant-tree encoding** (§2: `name\0slot`, bare dirs, mode tags x/l/m, `lanes` bitmap child, omit all-ones, non-identity meta, always-variant) | **IMPLEMENTED (library) / UNWIRED** | `layer.rs:130 file_key`, `:221 variant_node`, `:238 encode_lane`, `:368 encode_union`. Node shapes exactly per spec; `tree_oid_matches_real_git_constants` (`layer.rs:1454`) anchors the hashing to git's real empty-tree/blob/tree oids. **Gap:** the shipping store never emits these bytes (Path A uses plain trees). |
| 2 | **Two orders** (§3: `container_cmp` authoritative + never re-sorted; git `base_name_compare` only for reconstruction; clean names, no sort byte) | **IMPLEMENTED** | `layer.rs:173 container_cmp`, `:193 entry_cmp`; `container_cmp_is_bytewise_on_keys` (5000-case fuzz), `file_vs_dir_same_name_both_orders`. Names stay clean; the file/dir divergence is synthesized at compare time, not baked in. |
| 3 | **Two merges / holes** (§4: compose delta∘delta holes survive; overlay delta∘full holes dissolve) | **IMPLEMENTED** | `frame.rs:24 compose`→`depot::stream::compose_stream`; `frame.rs:75 / unionstore.rs:287 overlay_full`. Hole survival/dissolution is delegated to the `depot` codec primitives and exercised by frame/shard lifecycle tests. |
| 4 | **Geometric stack** (§7: push, merge while top ≥70% of next, ~log(n)) | **IMPLEMENTED** | `geostack.rs:56 push` (integer `10·top ≥ 7·next`), `:78 collapse`; `the_70_percent_boundary`, `equal_sizes_stay_shallow`, `preserves_apply_order_and_bytes`. |
| 5 | **Seal lifecycle** (§7/§8: seal replaces refPrefix with collapsed union AND keeps folded history; §5.3 as **reverse** deltas; old f1 → cold verbatim) | **PARTIAL / DIVERGES** | `frame.rs:88 seal` collapses stack→refprefix but **discards** the folded history (RAM-only). `unionstore.rs:32-37` + `IMPL-NOTES.md #1` **explicitly defer** the persisted seal that rewrites BASE and re-encodes history as reverse deltas. Path A `store.rs seal_f1` retires f1→cold verbatim (real), but over plain trees. No union-seal-with-reverse-history anywhere. |
| 6 | **Reslot-by-oid** (§5.2: match variants by `(mode,oid)` read FREE from lane trees; bitmap-only updates; never hash stored content) | **IMPLEMENTED (library) / UNWIRED** | `layer.rs:385 current_variants` gets oid from `old_lanes` (`:398-402`, "never by hashing the stored content"); `:432 delta_multi_lane_stacked` matches by id, emits `lanes`-only child when only the bitmap moved (`:463-473`), holes vanished variants (`:482-485`). **Minor divergence:** new variants take `next_slot = max+1` (`:453`), not the spec's "lowest free slot / most-lane-overlap" heuristic (§9 permits swapping tunables, so tolerable). **Gap:** unused by the mirror. |
| 7 | **Streaming current-state** (§5.1/§10: read base+stack in lockstep, no union materialized to make a delta) | **IMPLEMENTED** | `layer.rs:513 visit_current` walks base + stack cursors in lockstep; union built only in `frame.rs:65 union` / `unionstore.rs:266 union` for a read/seal. Matches "materialize only for a read or a seal." |
| 8 | **Persistent ancestry lanes** (§6: first-parent frozen, fork if taken; compaction reuses dead indices → width = peak concurrent) | **IMPLEMENTED (library) / UNWIRED** | `lanes.rs:52 assign_lanes`, `:91 compact_lanes`; `append_only_prefix_agrees_with_full`, `compact_reuses_freed_indices`. **Gaps:** stale-ref (~month) retirement and delta-of-delta among lanes (§6.1) not present (§6.1 defers the latter). Not called by the importer. |
| 9 | **Reflog #lanes ≥ #refs** (§6) | **IMPLEMENTED (library) / UNWIRED** | `reflog.rs:57` asserts `live_lanes() >= refs.len()`, panics on violation; `too_few_lanes_panics`. **But** this is an in-RAM `Vec<LayerEntry>`, distinct from Path A's persisted `REFLOG` chain (`store.rs`); the two reflogs are unrelated. Not wired to git. |
| 10 | **Sharding** (§9: route by top bits of path hash; lockstep advance, empty deltas OK; one thread/shard; oid shard-count-invariant) | **IMPLEMENTED (in-memory) / NOT PERSISTED** | `shards.rs:36 shard_of`, `:72 advance` (all shards every rev via `thread::scope`), `:93 reconstruct_tree_oid` gathers from every shard; `sharding_is_transparent_and_sha_exact` proves oid invariance across 0–3 bits. **Gap:** `Shards` is `Vec<Frame>` in RAM; "one `UnionStore` per shard" is deferred (`IMPL-NOTES #3`). |
| 11 | **VBF f0/f1 persistence serving historical refs SHA-exact** (the headline: §8 + §5.3, the union encoding persisted and any historical ref served from stored bytes) | **PARTIAL / DIVERGES — the halves don't meet** | **Path A** genuinely does this for *plain trees*: `store.rs` reverse-delta TREES (f0 full, older reverse), `seal_f1`, stable N−1−k indices; `readout.rs:49 for_commit` walks head→k applying reverse deltas and serves any commit's tree SHA-exact from bytes (roundtrip test). **Path B `UnionStore`** persists+re-serves from bytes (reopen tests `unionstore.rs:426,506` are genuine, not in-memory) **but**: (a) **forward** deltas from an **oldest** base (`seed`→BASE f0 = oldest; `union_at` composes ALL deltas forward) — the explicitly-rejected "encode all your deltas backwards / whole implementation broken" direction (§5.3, C59); so current-tip read is O(history) and there is no rebase/seal to reset the base; (b) fed **synthetic** lanes, **never real git**; (c) not sharded, not reflog/lane-wired. **No test or code path persists the union/lane encoding of REAL git via reverse-delta VBF and serves a historical ref from it.** |

## On Agent 4's report

Agent 4's numbers are accurate (39 lib + 33 integration green, zero warnings)
and its new test `persisted_seals_to_cold_and_reconstructs_every_revision`
(`unionstore.rs:506`) is a **real** strengthening — it hard-asserts the cold
file grew, so `seal_f1`/`cold_iter`/multi-record `stream_frame_records` are
genuinely exercised, not faked. That claim holds up.

**What Agent 4 overstated:** "the property the user cares about … is verified
across all storage tiers." It is verified for `UnionStore` **on synthetic
lanes, in the forward-delta direction**. It is **not** verified that the
designed encoding persists and serves **real git** history, nor in the
**reverse-delta** direction the design mandates. The forward-delta deferral is
honestly documented (`IMPL-NOTES #1`, `unionstore.rs:32`) — it is not a hidden
shortcut — but the pipeline's "done/verified" framing papers over that the
union path is an **unwired island** and that the shipping mirror doesn't use the
union encoding at all.

## Bottom line (blunt)

**Not a sandcastle, but not the design either — it's two half-bridges that
don't touch.** Every pillar exists as correct, tested code; nothing is faked;
the real-git and persist-from-bytes properties are each demonstrated. **But they
are demonstrated on different code paths.** The design is a single system: the
variant/union/lane encoding (§2/§5/§6) persisted as reverse-delta VBF (§8) and
serving any historical ref from those bytes. The implementation split it: the
git-wired store persists **plain per-commit trees** (no variants, no lanes), and
the variant/lane engine's only persistence (`UnionStore`) runs **forward
deltas** on **synthetic** input and is **called by nothing but its tests**.

**Single most important thing still not matching the design:** the store does
not persist-and-serve the **designed encoding** for **real git** — the
union/lane variant tree is never written by the mirror, and where it is written
(`UnionStore`) it uses the explicitly-forbidden **forward-delta / oldest-base**
direction (§5.3, C59) with no seal/rebase, so the headline deliverable ("any
historical ref's lane tree served byte-exact from stored VBF frames") is
realized only as two disconnected demos, not one working pipeline.
