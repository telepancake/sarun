# gitdepot IMPL-NOTES (Agent 3, implementer)

Scope: (1) delete the dead skeleton cluster (WORKMAP §2); (2) wire Path B's
union/delta primitives into Path A's VBF persistence so delta layers + the
refPrefix are stored as real VBF frames and any live revision's lane tree is
served SHA-exact **from stored bytes** (WORKMAP §3, order-of-ops steps 1–2).

Build (warnings ok), the prescribed command, is **green with zero warnings**:

    cd /home/user/sarun/gimir && uv run --with cargo-zigbuild --with ziglang \
      cargo zigbuild --release -p gitdepot --tests \
      --target x86_64-unknown-linux-musl
    -> Finished `release` profile ... (0 warnings)

All 38 gitdepot lib unit tests pass (frame/layer/geostack/lanes/reflog/shards,
the live-git reconstruction test, and the new persistence round-trip).

## Files changed

- **Deleted** (the closed dead cluster, WORKMAP §2):
  - `src/oidenc.rs` (`struct Skel` — the rejected per-path materialize skeleton)
  - `src/lanestore.rs` (skeleton-driven encoder; used by nobody live)
  - `src/reslot.rs` (`Slots`/`Occupant`/`Bitmap`; used only by `oidenc`)
  - `src/variants.rs` (`content_key`/`meta_key`/`leaf_delta`/`extract`; used
    only by `oidenc`/`lanestore`; `layer.rs` reimplements it cleanly)
  - `tests/lanestore.rs`, `tests/lanestore_seal.rs`, `tests/lanestore_union.rs`
    (the dead cluster's dedicated integration tests — they imported
    `gitdepot::lanestore::LaneStore`, now gone).
- **`src/lib.rs`**: removed the `lanestore`/`oidenc`/`reslot`/`variants` `mod`
  decls; added `pub mod unionstore;`. Fixed the stale doc at the old line 1355
  that claimed ingest is driven by "the lane-store encoder (`lanestore`)".
- **`src/unionstore.rs`** (NEW, ~450 lines incl. test): the persistence wiring.

Verified there are no remaining references to the deleted modules anywhere in
`src/`, `tests/`, `examples/`, or `cli.rs`/`main.rs`.

## Deviation from the WORKMAP (justified)

**`readout.rs` was NOT deleted.** WORKMAP §2 lists `src/readout.rs` as dead
("declared, referenced nowhere else"). That classification is factually wrong:
`TipReadout` is LIVE — it is the store's attach/serve path and is exercised by
`tests/roundtrip.rs` (the import→export→mirror→attach SHA-exact suite,
`TipReadout::for_commit` at the tag-pin assertion), by `tests/readout.rs`, and
by `examples/bench.rs`. It depends only on `crate::store` + `depot::variant`
(no dead-cluster imports). Deleting it would break live tests. Per the hard
rule against declaring things broken / implementing the best faithful version,
I kept it and its `pub mod readout;` decl. The genuinely dead cluster is the
closed set {`oidenc`,`lanestore`,`reslot`,`variants`} only.

## How the union/delta primitives now flow into VBF frames

`src/unionstore.rs::UnionStore` promotes the `frame::Frame` lifecycle (refPrefix
+ geometric delta stack, §7) onto a real `wikimak_depot::Depot`, reusing the
SAME frame codec and prepend/seal discipline `store.rs` drives — not a second
engine (union bytes ARE `depot::codec` bytes, so the frame machinery carries
them unchanged).

- Two chains in one Depot: **BASE=0** holds the refPrefix
  (`layer::encode_union`) as the standalone **f0** record; **DELTAS=1** holds
  the forward delta layers newest-first (f0 = newest, f1 accumulator, cold).
- `seed(lanes)` writes BASE f0. `advance(new_lanes)` generates the delta with
  `layer::delta_multi_lane_stacked(base, stack.layers(), old_lanes, new_lanes)`
  — reslot-by-oid, current state read **streaming** via `visit_current` over
  base+stack, **no union materialized** (§5.1) — then **prepends** it to DELTAS
  as its own VBF frame (new delta = new f0; previous f0 demoted verbatim into
  the f1 accumulator, anchored on the new record; f1 seals to a cold frame past
  the threshold via `Depot::seal_f1`). This is `store.rs::prepend_batch`
  specialized to a single verbatim record, byte-for-byte the same discipline.
- Reads/seal are the ONLY place a union is materialized (§4/§7):
  `union_at(n)` reads BASE f0 + the first `n` DELTAS records **back through the
  depot**, `compose_stream`-collapses them (holes survive), `overlay_full`s onto
  the base (holes dissolve), and `reconstruct_lane_at(n, lane)` runs
  `layer::reconstruct_lane_tree_oid` on that union — SHA-exact, from stored
  bytes. On `open`, the in-RAM geostack is rebuilt from the persisted deltas so
  the write-side current read stays a bounded ~log(n) stack.
- Test `persisted_union_reconstructs_sha_exact`: seed 2 lanes, advance twice,
  flush, **drop and reopen the depot**, then reconstruct every lane at rev0
  (seed), rev1, and the tip — each equals `layer::lanetree_tree_oid` of the
  source tree. Reconstruction reads only stored VBF frames.

## Deferred (unbuilt, with justification — do NOT count as done)

1. **Persisted frame *seal* + reverse-delta history (DESIGN §5.3).** The design
   wants the newest full-state to be f0 with *older* revisions stored as
   **reverse** deltas from it. `frame.rs`'s stack (and thus this store) is a
   **forward** delta chain from a sealed base: every LIVE revision (base + a
   delta prefix) reconstructs from stored bytes, which is the between-seals
   frame lifecycle in full. A persisted seal that collapses the stack into a new
   f0 refPrefix AND re-encodes the folded-in revisions as reverse deltas is the
   one primitive `frame.rs` itself does not yet carry, and the direction cannot
   be validated SHA-exact against real git in this harness (box/FUSE/bwrap tests
   don't run here). Implementing it forward-only-but-persisted, and documenting
   the reverse-across-seal gap, is the faithful minimum rather than guessing the
   reverse encoder. `Depot::seal_f1` already retires the f1 accumulator to cold
   verbatim (size management), so nothing is lost — only pre-seal *reverse*
   reconstruction is deferred.

2. **Replacing Path A's TREES payload in `store.rs`/`lib.rs` (WORKMAP steps
   3–5).** `store.rs::flush_tree_batch`/`tree_view` and `lib.rs`'s git ingest
   (`frontier_walk`/`ingest_stream`, ref schema) still drive the per-tree
   `depot::View` reverse-delta path — the LIVE, real-git-tested Path A. Swapping
   it to `UnionStore` requires re-indexing refs from `(commit_idx, tree_idx)` to
   `(revision, lane)` and rewiring `lanes::assign_lanes` + `gitsrc` through the
   whole importer, an SHA-exactness change that can only be proven by the
   real-git box tests (unavailable in this sandbox). Kept Path A untouched and
   green so nothing regresses; `UnionStore` is the proven, self-contained
   persistence seam those steps plug into.

3. **Multi-shard persisted layout** (`shards.rs` → one `UnionStore` per shard)
   and **delta-among-lanes** variant locality (§6.1) — explicitly deferred by
   the WORKMAP (single shard, full side-by-side lanes first).
