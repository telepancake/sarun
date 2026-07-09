# gitdepot — Validation against DESIGN.md

Phase 4. Each design section is marked IMPLEMENTED / PARTIAL / MISSING against
the code that is actually *reachable from the CLI*, not merely present in a test
fixture. Evidence is `file:symbol` (or `file:line`).

> This supersedes an earlier VALIDATION.md written before the tree changed: that
> version predates the wiring of the `union` CLI command and references files
> since deleted (`geostack.rs`, `frame.rs`, `unionstore.rs`, `shards.rs`,
> `variants.rs`, `gitsrc.rs`, `reflog.rs`). Those files are gone; the current
> reachability picture is below.

## The two code paths (read this first)

`gitdepot/src/cli.rs::cli_main` dispatches to **two disjoint stores** that do
not share a corpus:

- **Legacy single-tree path** — `import` / `update` / `mirror` / `list` /
  `log` / `export`. Entry points `lib.rs::import_opts`, `lib.rs::update`,
  `lib.rs::mirror_opts`, `lib.rs::export`, backed by `store.rs::Store` /
  `store.rs::Ingest`. This stores **one git tree per commit** (`depot::diff(None,
  view)` over a single resolved view — `lib.rs::ingest_stream`). It has the
  commit/reflog/tag chains and `meta.sqlite`, but **no lanes, no variants, no
  union**. It is the pre-union design.

- **Union path** — `union` / `union-verify` (`cli.rs:154-176`). Entry points
  `lib.rs::union_import` → `lanestore.rs::LaneStore::encode_repo_union`, and
  `lib.rs::union_verify`. This is the code that realizes the lane/variant/union
  model of DESIGN §1–§8. It uses `oidenc.rs::Encoder`, `layer.rs`, `reslot.rs`,
  `lanes.rs`. It stores **only the tree union** (one chain, `lanestore.rs:51
  const CHAIN`) plus a `meta.sqlite` of ref→(sha,rev,lane); it has **no commit,
  reflog, or tag chain and no export**.

DESIGN.md describes the union model, so the union path is the conforming target.
But the union path is not a whole mirror (no ingest/fetch, no export, no
commit/reflog/tag persistence), and the whole-mirror commands run the legacy
path. **No single reachable command realizes the full design.** The context note
("implemented in more than one place… which conforms is to be determined") is
accurate: §10's chains live in the legacy `store.rs`; §1–§8's union lives in the
union path; the two are not integrated.

Files the context listed as "relevant" that **do not exist** in the tree:
`variants.rs`, `gitsrc.rs`, `frame.rs`, `unionstore.rs`, `reflog.rs`,
`shards.rs`. Their absence is the direct cause of the MISSING findings below
(reflog-in-union, geometric stack as a named unit, sharding).

---

## §1 Domain model: revisions, lanes, union — IMPLEMENTED (union path)

`lanes.rs::assign_lanes` freezes each commit's lane from its first parent;
`lanes.rs::compact_lanes` compacts monotonic ids into a reused index space.
`lanestore.rs::encode_repo_union` (lines 180–207) drives it: lane birth/death
from merge second-parents, `dying_at`, per-revision transitions. The union of
all live lanes' trees is held as one node tree by `oidenc.rs::Skel` /
`Encoder::advance`.

## §2 Variant/union tree encoding — PARTIAL (union path)

Implemented: file variant key `name\0varint(slot)` (`layer.rs::file_key`, marker
documented lines 8–9); bare directory nodes; mode-tag children `x`/`l`/`m`
(`layer.rs::TAG_EXEC/TAG_SYMLINK/TAG_GITLINK`, applied by
`oidenc.rs::set_mode_tag`); `lanes` bitmap as a child (`layer.rs::LANES`,
written by `oidenc.rs::variant_reverse_node` and `full_view_dir`); single
variant still stored as a variant; meta children non-identity.

Gap: the **all-ones `lanes` omission** (§2: "omitted when the bitmap is
all-ones") is **not applied in the reachable union encoder**. `oidenc.rs:304`
states outright "the all-ones omission is a size optimization not applied here,"
and `variant_reverse_node`/`full_view_dir` always emit the bitmap. The omission
is only supported by `layer.rs::variant_node` (`bitmap = None`), which the union
path does not use. So the common-case size win of §2 is unrealized on the live
path.

## §3 One order — IMPLEMENTED

The codec's `container_cmp` bytewise order is the single walk order; git trees
are reordered on load into the tree cache (`oidenc.rs::parse_tree` +
`lanestore.rs::Cat` caching). Note the §3 clause "a blob's shard is assigned
during that same cache load" is inert — sharding does not exist (§9).

## §4 overlay / compose / delta — PARTIAL

The three streaming byte-level ops exist and are used: `depot::stream::
overlay_full`, `compose_stream`, `diff_stream` / `diff_stream_holes`
(`depot/src/stream.rs:561/251/507/518`). Tombstone vs hole semantics
(hole dissolves on overlay, survives on compose) are implemented and unit-tested
in `layer.rs`. **Gap:** the "mmap updater… single pass… `MADV_DONTNEED` on
consumed front regions" adapter is **MISSING** — `MADV_DONTNEED`/`madvise`
appears only in doc comments (`depot/src/stream.rs:16,36`); the real ops are
plain `Vec`-building, no mmap driver.

## §5 refPrefix + geometric delta stack — MISSING (from reachable code)

The reachable union path does **not** implement the geometric stack. There is no
push-and-compact with the "top ≥ 70% of the entry below" rule anywhere in the
tree (grep for `70`, `0.7`, `geostack`, `GeoStack` finds only a doc phrase at
`layer.rs:657` and unrelated numbers). `encode_repo_union` instead accumulates
reverse-delta records in a flat `batch` and seals when `batch_bytes >=
batch_ram_bound()` (`lanestore.rs:225-323`, `seal_prepend`). "Current state =
refPrefix + live stack read by lockstep" is likewise absent from the live path:
the union encoder tracks state in the in-RAM `oidenc.rs::Encoder`, not by
lockstep over refPrefix + a delta stack. The `stack`-parameterized functions
that would support §5 (`layer.rs::delta_multi_lane_stacked`,
`current_variants_ref`, `visit_current`) are reachable **only from `layer.rs`
unit tests** (all call sites are in `#[cfg(test)]` blocks, lines 1362–1692);
`encode_repo_union` never calls them. §5 is thus not present in reachable code.

## §6 Delta generation (reslot) — IMPLEMENTED (union path)

Per-path variant algebra factored out as `reslot.rs` (`Slots::reslot`,
`SlotChange`, most-shared-lanes freed-slot reuse). Variant identity by git
`(mode, object id)` with no content hashing: `oidenc.rs::VarKey` /
`new_variant_set` / `variant_reverse_node` compare `bo.id.1` (oid) and mode.
Bitmap-only updates emitted when membership changes but oid does not
(`variant_reverse_node` `(Some,Some)` arm). The tree-wide lockstep is driven by
`Encoder::advance` over `Trans` transitions. Note: this path reslots against the
encoder's live skeleton rather than the §6 "lockstep of three iterator sets over
refPrefix+stack," because §5's stack is absent — the *result* conforms, the
stated mechanism differs.

## §7 Newest full, older reverse — IMPLEMENTED (union path)

f0 is the newest full state (`encode_repo_union` i==0 seeds a positive full
record, lines 298-303); every older revision is a reverse delta from the newer
(`Encoder::advance` returns reverse-delta records; `apply_reverse_record` /
`combined_at` reconstruct older by folding reverse deltas newest→oldest).
Removals encoded as holes and converted to tombstones over the empty backdrop
(`lanestore.rs::holes_to_tombstones`).

## §8 Lanes: assignment and lifecycle — PARTIAL (union path)

Implemented: first-parent lane freezing, monotonic ids, compaction with reuse so
a bitmap is only as wide as peak concurrency (`lanes.rs::assign_lanes`,
`compact_lanes`; tests `compact_reuses_freed_indices`,
`merge_ends_the_second_parent_lane`). Lane death on merge (second-parent) is in
`encode_repo_union` (death array, `dying_at`, lines 191-206).

Gaps:
- **`#live lanes ≥ #live refs` is not enforced.** Lanes are assigned purely by
  commit ancestry; two refs pointing at the same commit share one lane, and
  nothing guarantees a distinct lane per ref. No symbol establishes the
  invariant.
- **Inactivity retirement is MISSING.** "no new commit for a long stretch →
  declared inactive → retired into the reflog" has no implementation — there is
  no reflog in the union path at all, and no inactivity/staleness logic (grep
  `inactive`/`stale` in `lanes.rs`/`lanestore.rs` finds nothing).
- "Choose the advancing commit's lane to minimize the diff to the previous
  frame" is not done; lane choice is fixed by ancestry only.
- Synthetic-hidden-ref exclusion is not present on this path.

## §9 Sharding — MISSING

No implementation anywhere. There is no `shard-bits` parameter, no path-hash
routing, no per-shard thread, no re-shard. `shards.rs` does not exist. The only
occurrences of "shard" in gitdepot are aspirational doc comments
(`layer.rs:1000`, `layer.rs:1018`); the sharding code under `wikimak/media` is an
unrelated MediaWiki path layout. Import is single-threaded
(`encode_repo_union` is a plain `for i in 0..n_rev`). §3's shard-on-load and
§8's cross-shard gather are correspondingly inert.

## §10 Persistence: VBF chains — PARTIAL / split

- **Legacy path (`store.rs`)**: has the per-chain frame discipline (f0
  standalone, f1 anchored on f0, cold seal past threshold), newest-first stable
  indices numbered from the oldest end, `meta.sqlite` of current refs, and a
  **reflog chain** (`store.rs::ReflogRecord`, `REFLOG`, `reflog()`), plus commit
  and tag chains. But its tree state is **one tree per commit, unsharded** — not
  the §9/§10 "sharded tree state per shard."
- **Union path (`lanestore.rs`)**: implements the same frame discipline for the
  tree-union chain (f0/f1/cold via `seal_prepend`, `walk_records`, stable
  newest-first indices, `meta.sqlite` refs). But it has **only the one tree
  chain** — **no commit chain, no reflog chain, no tag chain**, and the tree
  chain is **not sharded**. The mandatory local reflog and non-fast-forward
  recording of §10 are absent here.

So §10 is realized in pieces across the two stores and by neither in full; the
"per-shard tree + separate commit/reflog/tag chains" combination exists nowhere.

## §11 Ingest / fetch — PARTIAL (legacy path only)

The negotiation-driven fetch (withhold-boundary, metadata-first, single batch,
no bare-clone-as-store) lives on the legacy path: `mirror_opts` / `update` and
the `dag_scope`/negation machinery in `lib.rs`. The **union path does not fetch
at all** — `encode_repo_union` reads an already-present local repo via `git log`
/ cat-file (`lanestore.rs:211`, `Cat`), so §11 does not apply to the conforming
store. Signed-commit/annotated-tag SHA-exactness is explicitly *unsupported* on
export (`lib.rs::export` errors on `extra_headers`).

---

## Summary

**Conforms (union path, reachable via `union`/`union-verify`):** §1 domain
model, §3 one order, §6 reslot delta generation, §7 newest-full/older-reverse.
These are solid and SHA-exact-verified by `union_verify` →
`LaneStore::tree_oid_at`.

**Partial:** §2 (all-ones bitmap omission not applied on the live encoder), §4
(byte ops present; mmap/`MADV_DONTNEED` adapter missing), §8 (lane assignment and
compaction yes; `#lanes≥#refs`, inactivity retirement, diff-minimizing lane
choice no), §10 (frame discipline yes, but split across two stores and neither
sharded/complete), §11 (fetch on legacy path only, not the union store).

**Missing entirely:** §5 geometric delta stack (union path uses a flat
batch/seal; the stack-based functions are test-only), §9 sharding (no
implementation, doc comments only). Both correspond to recently-deleted files
(`frame.rs`/`unionstore.rs`, `shards.rs`) named in the context but absent from
the tree.

**Structural gap:** the design is a single integrated mirror; the code is two
disjoint stores. The whole-mirror commands (`import`/`update`/`mirror`/`export`)
run the legacy single-tree store (reflog/commit/tag chains, no union); the union
model runs only under `union`, which has no fetch, no export, and no
commit/reflog/tag persistence. No reachable command realizes DESIGN.md end to
end.
