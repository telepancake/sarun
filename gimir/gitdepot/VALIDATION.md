# gitdepot — VALIDATION (Phase 4)

Design (`DESIGN.md`) checked section-by-section against the current code. Each
verdict is IMPLEMENTED / PARTIAL / MISSING with `file:symbol` evidence.
Skeptical throughout: a part only counts as IMPLEMENTED for a code path if the
**real, reachable** path realizes it, not merely a test fixture or an off-path
module.

## The decisive reachability fact

The CLI (`cli.rs:cli_main` → `import`/`update`/`mirror`/`export`) reaches **one**
engine: `lib.rs` → `store::Ingest` → `wikimak_depot::Depot`, storing **one git
tree per commit** (`lib.rs:tree_layer` builds a plain `path → (blob, mode-attr)`
`depot::Layer`; the TREES chain reverse-deltas between chain-neighbouring trees).
Grepping the live path proves it: `lib.rs` and `store.rs` contain **zero**
references to `UnionStore`, `LaneStore`, `encode_union`, `delta_multi_lane`,
`assign_lanes`, `Shards`, or `Frame::` — the entire union/lane/shard machinery is
unreachable from the binary.

The union-of-lanes design (§1/§2) lives only in two **off-path** clusters, wired
to each other and their own tests but never to git:

- **B (union engine, §2-correct encoding, forward deltas):** `layer.rs` +
  `geostack.rs` + `frame.rs` + `unionstore.rs` + `shards.rs` + `lanes.rs` +
  `reflog.rs` + `gitsrc.rs`. Reachable only from inline unit tests and
  `gitsrc.rs`'s test.
- **C (reverse-delta union over a `\0v/\0m` nested encoding):** `lanestore.rs` +
  `oidenc.rs` + `variants.rs` + `reslot.rs` (+ shared `lanes.rs`). Reachable only
  from `tests/lanestore*.rs`.

So the design's *headline* — "the state stored at each revision is the union of
the git trees of all live lanes" — is **not realized in the shipping path**, and
no single engine is both §2-encoding-correct and §7-direction-correct.

---

## Per design section

### §1 Domain model (revisions, lanes, the union)
**Live path: MISSING. Off-path: PARTIAL (B/C).**
- Live: `store.rs` TREES chain stores a single git tree per commit
  (`lib.rs:tree_layer`, `lib.rs:delta_layer`). There is **no union of live
  lanes** at a revision.
- Off-path: `lanes.rs:assign_lanes` + `compact_lanes` realize "one live line per
  concurrently-live branch, lane dies when its line ends, bitmap width = peak
  concurrent"; `layer.rs:LaneTree` / `oidenc.rs` treat a lane as one live tree in
  a union. These are correct but unreachable from the binary.

### §2 Encoding: the variant/union tree — realized twice, and they disagree
**`layer.rs`: IMPLEMENTED (but off-path). `variants.rs`: DIVERGES.**
- `layer.rs` matches §2 point-for-point: `file_key` = `name\0varint(slot)` as a
  single node; `dir_key` = bare name; `variant_node` puts content in the node
  blob, mode as an `x`/`l`/`m` **mode-tag child** (`Mode::tag`), and a `lanes`
  child **omitted when all-ones** (`bitmap: Option`, `None ⇒ omit`, documented at
  `layer.rs:1`); meta children carry a non-identity `Set` blob so compose/overlay
  won't prune them; `container_cmp` keeps bare-dir vs `\0`-led-file unambiguous.
- `variants.rs` **contradicts §2**: it nests two children `\0v<slot>` (content)
  and `\0m<slot>` (meta) **under a wrapper node named by the path**
  (`variants.rs:content_key`/`meta_key`), where §2 requires **sibling** nodes,
  not a wrapper. It stores **mode as an attr** on `\0m` (`variants.rs:meta_view`)
  rather than as a mode-tag child, and it writes the `\0lanes` bitmap
  **unconditionally** (`meta_view` always inserts `LANES`), never omitting it for
  all-ones. Reachable only via cluster C.

### §3 One order
**IMPLEMENTED (off-path, `layer.rs`).**
- `layer.rs:container_cmp` is the single authoritative container order over clean
  names; `layer.rs:entry_cmp` reproduces git `base_name_compare` used **only** to
  reconstruct a tree for hashing. Git trees are sorted into container order once
  on cache load (`oidenc.rs:parse_tree`). The slot tag is excluded from the
  compare. C reuses `depot` codec `BTreeMap` order; not an independent §3 concern.

### §4 Operations: overlay / compose / delta
**Streaming ops: IMPLEMENTED (shared `depot` crate). mmap/MADV updater: MISSING.**
- `depot::stream` provides `compose_stream` (delta∘delta, holes survive),
  `overlay_full` (delta∘full, holes dissolve), and the reverse `diff`; tombstone
  vs hole are modeled (`depot/src/stream.rs`, `variants.rs:leaf_delta` uses
  `Node::hole`). Used by B (`unionstore.rs:compose`, `frame.rs`) and C.
- The §4 "mmap updater … single pass, `MADV_DONTNEED` on consumed front regions"
  is **implemented nowhere**: both engines compose in-RAM `Vec<u8>`
  (`unionstore.rs:compose`). The only `mmap`/`MADV` token in the tree is a
  comment at `layer.rs:1026`. MISSING.

### §5 Frame model: refPrefix + geometric delta stack
**IMPLEMENTED (off-path, B). Absent from the live path.**
- `geostack.rs:GeoStack::push` implements the 70% compaction with integer math
  (`10*top < 7*next` ⇒ stop) and `collapse`; `frame.rs:Frame` / `unionstore.rs`
  hold refPrefix + live stack and read current state by lockstep
  (`layer.rs:visit_current`) without materializing a union; `frame.rs:Frame::seal`
  = collapse + `overlay_full`. Correct, but unreachable from the binary; the live
  store uses the VBF f0/f1 accumulator (§10) instead of a §5 geostack.

### §6 Delta generation (write side) — realized twice
**IMPLEMENTED (off-path), duplicated.**
- B: `layer.rs:delta_multi_lane_stacked_oid` does the lockstep-of-three over
  refPrefix+stack (`current_variants_ref`) and the lane trees, matching variants
  by `(mode, oid)` read from the trees, never hashing stored content; a
  bitmap-only change emits a bitmap update; a new oid takes a freed/lowest slot.
  The oid-addressed `Objects` boundary (fetch blob only when a fresh variant is
  emitted) is present (`layer.rs:Objects`, `MemObjects`), matching §6's
  no-content-in-RAM requirement — the change described in `IMPL-NOTES.md`.
- C: `reslot.rs:Slots` is the same §6 per-path algebra as a **second** copy
  (`common_lanes` = most-shared-lanes among freed slots), used only by
  `oidenc.rs:advance_node`, which adds genuine subtree-prune-by-oid.

### §7 Delta direction: newest full, older reverse — no engine is both §2 and §7
**Live (A): IMPLEMENTED. C: IMPLEMENTED. B: DIVERGES.**
- Live: `store.rs` TREES — f0 is the newest tree in full, older records are
  reverse deltas from the newer; `readout.rs:TipReadout` walks head→target
  applying reverse deltas. Correct direction (over single trees, not a union).
- C: `lanestore.rs` (module doc + `oidenc.rs:Encoder::advance` via `Trans`) emits
  reverse deltas; full-state only at seal (`lanestore.rs:apply_reverse_record`).
- B: `unionstore.rs` stores **forward** deltas over an old refPrefix base — its
  own doc (`unionstore.rs:32`) marks reverse-at-seal **DEFERRED**. So the
  §2-correct engine has the wrong delta direction, and the §7-correct engines
  each miss §2 (A has no union; C has the §2-violating encoding).

### §8 Lanes: assignment and lifecycle
**Assignment/compaction: IMPLEMENTED (off-path). Inactivity retirement: MISSING.
Live refs carry no lane: DIVERGES.**
- `lanes.rs:assign_lanes` (first-parent freeze; earlier sibling forks a fresh
  lane; monotonic ids) + `compact_lanes` (free-on-death reuse, width = peak
  concurrent) match §8; `reflog.rs:LayerEntry` keeps #live lanes ≥ #live refs.
  "Minimize-frame-delta" = the first-parent continuation rule (documented at
  `lanes.rs:39`).
- **Inactivity retirement** ("no new commit for a long stretch → declared
  inactive, retired into the reflog") is **absent**: `assign_lanes` ends a lane
  only at its last in-scope commit; there is no staleness clock. MISSING.
- Live: `store.rs`/`meta.sqlite` refs carry **no lane id** (single-lane
  degeneracy) — refs point at a commit+tree index only.

### §9 Sharding
**PARTIAL (off-path, in-RAM only). Not persisted, not wired.**
- `shards.rs:Shards` splits by the top `shard_bits` of a stable full-path hash
  (`path_hash` FNV-1a, `shard_of`, `split`), reconstructs a lane across shards,
  advances shards in lockstep, and preserves the tree oid across shard counts —
  all §9. **But each shard is an in-RAM `frame::Frame`** (`shards.rs:32`
  `frames: Vec<Frame>`), never persisted to a per-shard Depot, and is invoked
  only from `gitsrc.rs`'s test. The live store and `lanestore.rs` are
  single-shard with no `shard-bits`. §9 is unrealized in anything persisted.

### §10 Persistence: the VBF chains
**IMPLEMENTED (live, A). Duplicated by two off-path drivers.**
- Live: `store.rs` drives four chains `TREES/COMMITS/REFLOG/TAGS` over one
  `wikimak_depot::Depot`: f0 standalone, f1 anchored on f0, `seal_f1`/cold
  verbatim past `seal_threshold()`, `prepend_batch` (batch-not-split), oldest-end
  stable indices (`frame idx = N-1-k`), no `deleted_at`; `meta.sqlite` holds
  current refs only, superseded refs go to the REFLOG chain. This section is real
  and correct on the shipping path.
- Two more §10 drivers exist off-path (`unionstore.rs`: BASE+DELTAS chains;
  `lanestore.rs:seal_prepend`: a single chain). Correct machinery, but §10's
  "one depot per mirror" is nominally violated by having three prepend/seal
  engines in the tree (only one reachable).

### §11 Ingest / fetch
**IMPLEMENTED (live, A), substantially. `gitsrc.rs` is a toy seam.**
- Live: `lib.rs` runs its own linearization (`walk_order`), one
  `git rev-list --parents` + one `git diff-tree --stdin` + one persistent
  `cat-file --batch`, with frontier-style per-commit views. The **withhold-the-
  boundary** fetch is realized as the shallow-stub contract (`lib.rs:1766` THE
  STUB CONTRACT; `shallow` boundary written so the server resends boundary tips
  and their trees); one-batch initial pull with a tag-wave ladder; no persistent
  bare clone; the tag chain is handled (`ingest_tags`, `TagPeel`); a
  non-fast-forward upstream is new records + a repoint, never a destructive
  replace (`lib.rs:67`, `1563`). "Metadata-first, then blobs" is realized
  pragmatically (ls-tree/cat-file batch; a `--filter=tree:0` wave is noted as
  hazardous at `lib.rs:2376`) rather than as an explicit two-phase reachable-set
  plan — PARTIAL on that one clause.
- `gitsrc.rs:read_commit_tree` is an `ls-tree`+`cat-file` source feeding
  `LaneTree`s: the B integration seam, not the negotiated fetch.

---

## Summary

**Conforms.** The persistence, delta-direction, and ingest of the *shipping*
one-tree-per-commit store (§7, §10, §11) are real and correct on the reachable
CLI path (`lib.rs`/`store.rs`/`readout.rs`). SHA-exactness round-trips through
`export`. The §2 variant encoding (`layer.rs`), the §5 geostack, the §6 reslot,
and the §8 lane assignment/compaction are each individually correct.

**Does not conform.**
1. **The core union model (§1/§2) is absent from the live path.** The binary
   stores one git tree per commit; it never builds the union of live lanes. Every
   union/lane/shard module is unreachable from `cli.rs`.
2. **No single engine is both §2-correct and §7-correct.** B (`layer.rs` +
   `unionstore.rs`) has the §2 encoding but forward deltas (reverse-at-seal
   DEFERRED, `unionstore.rs:32`). C (`lanestore.rs` + `variants.rs`) has reverse
   deltas but a `\0v/\0m` nested-wrapper encoding that violates §2 (wrapper node,
   mode-as-attr, bitmap never omitted for all-ones).
3. **Three parallel §10 frame drivers** exist (`store.rs`, `unionstore.rs`,
   `lanestore.rs`); the design wants one depot per mirror.
4. **Unimplemented anywhere:** §4 mmap/`MADV_DONTNEED` single-pass updater; §8
   lane inactivity retirement; §9 *persisted* sharding (`shards.rs` is in-RAM
   only). §11 metadata-first two-phase planning is only partial.

The path to conformance is unchanged from `WORKMAP.md`/`IMPL-NOTES.md`: converge
on `layer.rs`'s §2 encoding, give `unionstore.rs` the §7 reverse-at-seal
direction, fold the union payload into `store.rs`'s proven §10/§11 chains, then
delete cluster C. The oid-addressed encoder boundary (§6) that this needs already
landed in `layer.rs`; the remaining steps are the large, still-undone ones.
