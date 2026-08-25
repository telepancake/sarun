# gitdepot WORK MAP

Mapped against `DESIGN.md` (authoritative, current). Verdicts are CONFORMS /
PARTIAL / DIVERGES with `file:symbol` and the design section. Where a design
part is implemented in more than one place, the conforming one is named and the
others are marked as removal candidates against the exact point they violate.

## The three implementations on disk

The tree holds **three** encoders of the same history, only one of which is
wired to real git:

- **A — LIVE / one-tree-per-commit (the shipped scaffold).**
  `lib.rs` → `store.rs` → `wikimak_depot::Depot`, plus `readout.rs`, `cli.rs`.
  Drives real git (import/update/mirror/export), four VBF chains, SHA-exact,
  tested end-to-end (`tests/roundtrip.rs`, `equivalence.rs`, `two_frames.rs`,
  `spill.rs`, `staging_size.rs`, `readout.rs`, `stub.rs`). Stores **one git
  tree per commit** as a reverse-delta record — it does **not** build the
  union of live lanes. Conforms to the persistence/direction/ingest sections
  (§7, §8-refs, §10, §11) but **diverges from the core §1/§2 union model**.

- **B — UNION ENGINE, §2-conforming encoding (not wired to git, not the live
  store).** `layer.rs` (encoding) + `geostack.rs` (§5 stack) + `frame.rs`
  (in-RAM lifecycle) + `unionstore.rs` (persisted union over a real Depot) +
  `shards.rs` (§9) + `reflog.rs` (§8/§10 reflog) + `gitsrc.rs` (§11 source) +
  `lanes.rs` (§8). Only inline unit tests + `tests/lanestore.rs` touches
  `lanes`. `unionstore` persists but stores **forward** deltas (reverse-at-seal
  explicitly DEFERRED — see its module doc), so it is PARTIAL on §7.

- **C — REVERSE-DELTA union over a nested `\0v/\0m` encoding.**
  `lanestore.rs` + `oidenc.rs` + `variants.rs` + `reslot.rs` (+ shares
  `lanes.rs`). A persisted reverse-delta union store off the git object store,
  tested by `tests/lanestore*.rs`. Conforms to §7's direction and §6's
  O(changed) reslot, but its **on-disk shape (`variants.rs`) contradicts §2**.

`lanes.rs` is the one module both B and C depend on. `lib.rs`/`store.rs`
reference **neither** B nor C.

---

## Per design section

### §1 Domain model: revisions, lanes, the union
- **CONFORMS (B/C):** `lanes.rs:assign_lanes` + `compact_lanes` realize "one
  live line per concurrently-live branch, lane dies on merge, bitmap width =
  peak concurrent". `layer.rs:LaneTree` / `oidenc.rs` treat a lane as "one live
  tree in the union".
- **DIVERGES (A):** `store.rs` (TREES chain, `flush_tree_batch`) stores a single
  git tree per commit — there is no union of live lanes at a revision. This is
  the live path yet it does not implement §1's "state stored at each revision is
  the union of the git trees of all live lanes". Removal/replacement candidate
  for the TREES *payload* (not the chain machinery).

### §2 Encoding: the variant/union tree — **covered in two places, they disagree**
- **CONFORMS (B):** `layer.rs:file_key` = `name\0<slot>` single node;
  `layer.rs:variant_node` puts content in the node blob, mode as an `x`/`l`/`m`
  **mode-tag child**, and a `lanes` child **omitted when all-ones**
  (`bitmap: Option`, `None ⇒ omit`); `layer.rs:dir_key` = bare name. This is §2
  point-for-point. `container_cmp` keeps bare-dir vs `\0`-led-file unambiguous.
- **DIVERGES (C):** `variants.rs` nests two children `\0v<slot>` (content) and
  `\0m<slot>` (meta) **under a wrapper node named by the path** — violating §2
  "stored as **sibling nodes, not nested under a wrapper**" and "File variant —
  node named `name` + `0x00` + `varint(slot)`". It also stores **mode as an
  attr** on `\0m` rather than an `x`/`l`/`m` mode-tag child (violates §2 "Mode"),
  and `variants.rs:meta_view` **always** writes the `\0lanes` bitmap, never
  omitting it for all-ones (violates §2 "the `lanes` child is omitted when the
  bitmap is all-ones"). **Removal candidate — `variants.rs` and everything built
  on it (`oidenc.rs`, `lanestore.rs`).**

### §3 One order
- **CONFORMS (B):** `layer.rs:container_cmp` (authoritative container order) and
  `layer.rs:entry_cmp` (git `base_name_compare` for reconstruction only);
  git trees reordered once on load (`oidenc.rs:parse_tree` + the cache). Names
  stay clean (no slot tag in the compare).
- C reuses `depot` codec order via `BTreeMap`; not independently a §3 concern.

### §4 Operations: overlay / compose / delta
- **CONFORMS (shared, `depot` crate):** `depot::stream::compose_stream`
  (delta∘delta, holes survive), `overlay_full` (delta∘full, holes dissolve),
  `diff_stream` / `diff_stream_holes` (reverse). Tombstone vs hole modeled
  (`depot/src/stream.rs`). Used by B (`frame.rs`/`unionstore.rs:compose`) and,
  as removal-side markers, by C (`variants.rs:leaf_delta` uses `Node::hole`).
- The §4 "mmap updater / MADV_DONTNEED single pass" is **not implemented**
  anywhere; both B and C compose in-RAM `Vec<u8>`. PARTIAL.

### §5 Frame model: refPrefix + geometric delta stack
- **CONFORMS (B):** `geostack.rs:GeoStack::push` (70% rule, integer
  `10·top ≥ 7·next`) + `collapse`; `frame.rs:Frame` and
  `unionstore.rs:UnionStore` hold `refPrefix` + live stack and read current
  state by lockstep (`layer.rs:visit_current`), never materializing a union to
  read/delta. `frame.rs:Frame::seal` = collapse + `overlay_full`.
- **DIVERGES/absent (A, C):** neither the live store nor `lanestore.rs` uses a
  §5 geometric stack; both lean on the VBF f0/f1 accumulator (§10) instead.
  Acceptable as a different altitude, but the §5 in-RAM geostack exists only in
  B, unwired to git.

### §6 Delta generation (write side) — **covered in two places**
- **CONFORMS (B):** `layer.rs:delta_multi_lane_stacked` does the lockstep-of-
  three-iterator-sets over `refPrefix`+stack (`current_variants`) and the lane
  trees; variants matched by `(mode, oid)` read for free from `LaneTree`, never
  by hashing stored content; `lanes`-only change emits a bitmap update; new oid
  → fresh slot. Matches §6.
- **CONFORMS (C), duplicate:** `reslot.rs:Slots::reslot` is the same §6 per-path
  algebra as a standalone unit (pass 1 id-match, pass 2 most-shared-lanes among
  freed slots `common_lanes`, pass 3 lowest free, pass 4 delete). Faithful to
  §6, but it is a **second copy** used only by `oidenc.rs`. Removal candidate
  once C is dropped; or promote it as the shared reslot and delete the inline
  copy in `layer.rs`. `oidenc.rs` adds the §6 "fetch content by oid only when a
  `\0v` node is emitted, prune unchanged subtrees by oid" — genuinely §6/§9
  O(changed) behavior that `layer.rs` currently lacks (it takes whole
  `LaneTree`s with content in RAM).

### §7 Delta direction: newest full, older reverse — **covered in two places**
- **CONFORMS (A):** `store.rs` TREES chain — f0 is the newest tree in full,
  older records are reverse deltas from the newer (`lib.rs` header; `readout.rs:
  TipReadout::for_commit` walks head→target applying reverse deltas).
- **CONFORMS (C):** `oidenc.rs:Encoder::advance` emits the reverse delta;
  full-state materialized only at seal (`lanestore.rs` module doc).
- **PARTIAL/DIVERGES (B):** `unionstore.rs` stores **forward** deltas over an
  old `refPrefix` base (its doc: reverse-at-seal per §7 is "DEFERRED"). So the
  §2-conforming engine has the wrong delta *direction*, and the §7-correct
  engines (A single-tree, C nested-encoding) each miss something else. **No
  single implementation is both §2- and §7-correct.**

### §8 Lanes: assignment and lifecycle
- **CONFORMS (B/C):** `lanes.rs:assign_lanes` (first-parent freeze, sibling
  forks a fresh lane, monotonic ids) + `compact_lanes` (reuse on death, width =
  peak concurrent). `reflog.rs` enforces **#live lanes ≥ #live refs**
  (`LayerEntry`). "Minimize-frame-delta" lane choice = the first-parent rule.
- **NOT implemented:** inactivity retirement ("no new commit for a long stretch
  → inactive, retired into the reflog") is absent — `lanes.rs` only dies a lane
  on merge/drop. PARTIAL on §8.
- **DIVERGES (A):** live refs in `store.rs`/`meta.sqlite` carry no lane id
  (single-lane degeneracy).

### §9 Sharding
- **PARTIAL (B):** `shards.rs:Shards` splits by top `shard_bits` of a stable
  full-path hash (`path_hash` FNV-1a, `shard_of`, `split`), reconstructs a lane
  across shards, lockstep advance, oid invariant across shard counts — all §9.
  **But it runs in-RAM `frame::Frame`s only**, never persisted (no per-shard
  Depot), and is unwired to `store.rs`/`unionstore.rs`. The hash is flagged an
  open placeholder (§9 "swappable"), which is fine.
- **DIVERGES/absent (A, C):** the live store and `lanestore.rs` are single-shard;
  no `shard-bits`. §9 unrealized in anything persisted.

### §10 Persistence: the VBF chains
- **CONFORMS (A):** `store.rs` — four chains `TREES/COMMITS/REFLOG/TAGS`, f0
  standalone, f1 anchored-on-f0, `seal_f1`/cold verbatim past
  `seal_threshold()`, `prepend_batch` (batch-not-split), oldest-numbered stable
  indices (`frame idx = N-1-k`), no `deleted_at`. `meta.sqlite` current-refs
  only; reflog chain for superseded refs. This section is real and correct.
- **CONFORMS (B), parallel:** `unionstore.rs` reuses the same discipline
  (`prepend_delta`, `seal_f1`, `cold_iter`, `stream_frame_records`) for BASE +
  DELTAS chains; tested to cold (`persisted_seals_to_cold_and_reconstructs_
  every_revision`). Correct machinery, but a **second frame driver** beside
  `store.rs` (and a third in `lanestore.rs`). §10 "pick one" is violated by
  having three.

### §11 Ingest / fetch
- **CONFORMS (A):** `lib.rs` — `walk_order` (own linearization), one
  `git rev-list --parents` + `diff-tree --stdin` + persistent `cat-file
  --batch`, frontier-style per-commit views; withhold-boundary fetch,
  metadata-first, one-batch initial pull, no bare clone, tag chain
  (`ingest_tags`, `TagPeel`). §11 substantially realized in the live path.
- **CONFORMS (B), toy:** `gitsrc.rs:read_commit_tree` is a `ls-tree`+`cat-file`
  source feeding `LaneTree`s — the integration seam, not the negotiated fetch.

---

## Removal candidates (contradict the design or duplicate a conforming part)

1. **`variants.rs`** — on-disk shape contradicts **§2** (nested-under-wrapper,
   `\0v/\0m` two-key split, mode-as-attr, bitmap never omitted for all-ones).
   The authoritative §2 encoding is `layer.rs`.
2. **`oidenc.rs`**, **`lanestore.rs`** — built entirely on `variants.rs`; carry
   the §2 violation. Their §6 subtree-prune-by-oid and §7 reverse-at-seal ideas
   are worth **porting onto `layer.rs`** before deletion (they are the two
   things `layer.rs`/`unionstore.rs` lack).
3. **`reslot.rs`** — a correct but **duplicate** §6 reslot used only by
   `oidenc.rs`; either delete with cluster C or make it the single shared reslot
   and remove `layer.rs`'s inline copy. One §6 implementation, not two.
4. **Duplicate frame drivers** — three §10 prepend/seal engines
   (`store.rs`, `unionstore.rs`, `lanestore.rs`). §10 wants one depot per mirror;
   keep `store.rs`'s and fold the union payload into it.

Not removal: `layer.rs`, `geostack.rs`, `lanes.rs`, `reflog.rs`, `shards.rs`,
`gitsrc.rs`, `frame.rs` (the §2-conforming primitives, unwired but correct);
`store.rs`, `lib.rs`, `readout.rs`, `cli.rs` (the live persistence/ingest).

## What is missing (no implementation anywhere)
- The union-of-lanes tree in the **live, persisted, git-driven** path (§1/§2):
  A stores single trees; B/C are not wired to git.
- A single engine that is **both** §2-encoding-correct **and** §7-direction-
  correct (newest full, older reverse). B has §2 but forward deltas; C has
  reverse deltas but the §2-violating encoding.
- §8 lane inactivity retirement; §9 persisted sharding; §4 mmap/MADV_DONTNEED
  single-pass updater.

---

## Summary + first thing to implement

**Summary.** The design's core — a persisted union of live lanes in the
`name\0<slot>` variant tree (§1/§2), stored newest-full/older-reverse (§7),
sharded (§9), driven by real git (§11) — is **not realized in one place**. What
ships (`lib.rs`+`store.rs`) is a §7/§10/§11-correct one-tree-per-commit scaffold
with **no union**. Two parallel union engines exist off the live path: **B**
(`layer.rs`+`unionstore.rs`) has the §2-correct encoding but forward deltas;
**C** (`lanestore.rs`+`variants.rs`) has §7 reverse deltas but an encoding that
**contradicts §2**. `layer.rs` is the single authoritative §2 encoding.

**First thing to implement:** collapse the encoding duplication in favor of
§2. Delete cluster C's `variants.rs` shape after **porting its two design-right
ideas onto `layer.rs`**: (a) `oidenc.rs`'s O(changed) *diff by tree-oid with
subtree pruning* (so the delta generator reads git trees by oid instead of
taking whole in-RAM `LaneTree`s), and (b) `lanestore.rs`'s *reverse-delta-from-
newest / full-state-only-at-seal* lifecycle (the §7 direction `unionstore.rs`
currently defers). Land that as the one union engine, then wire it as the TREES
payload in `store.rs`'s existing `prepend_batch`/`seal_f1` machinery — reusing
A's proven §10/§11 persistence and ingest unchanged. Concretely, the smallest
first commit: give `layer.rs:delta_multi_lane_stacked` an oid-addressed
`Objects`-style source (as `oidenc.rs` has) so it no longer needs full lane
content in RAM — the prerequisite for both the git wiring and deleting C.
