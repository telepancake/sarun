# The depot — design

Supersedes the open questions in `DEPOT-BRIEF.md`; the brief's directive
(name-addressed, tombstones first-class, hashing never in the interface)
stands and is assumed here. This document records the decisions from the
2026-07 design discussion. Where a decision has a cost, the cost is stated
next to it.

## 1. Definition

A **depot** stores a set of **layers** of hierarchical named data, merging
equal subtrees and blobs internally. Layer composition, ordering, and
inventory belong to the caller.

Every clause is load-bearing:

- **a set of layers** — one depot holds many layers; layers are the unit of
  ingest, export, diff, and deletion. The depot is not a single-tree store.
- **hierarchical named data** — NOT "a filesystem". A layer is a tree of
  nodes keyed by opaque byte names. It *can* be a filesystem image (a box
  overlay upper), or a structured/tabular dataset, or a git repository's
  snapshot, or a wiki. The word *path* is deliberately absent from this
  document: "path" smuggles in tar/POSIX assumptions (separators, modes,
  leaf-only content) that the model does not make.
- **merging equal subtrees and blobs internally** — structural sharing
  across the layers a depot holds is the depot's own responsibility and is
  invisible in the interface. How well a variant dedups is a quality
  property of the variant, not part of the contract (§7).
- **inventory belongs to the caller** — which layers exist, how they stack,
  which box/mirror/wiki owns them, fetch cooldowns, import watermarks,
  string→id dedup indexes: all bookkeeping, all outside the depot,
  typically in SQLite (§3).

## 2. Node model

```
Node = name            opaque bytes; ordering/semantics defined by the variant's domain
     + blob?           optional content bytes — interior nodes may carry content
     + children        ordered by name
     + presence        live | tombstone
     + opaque?         on branch nodes: masks lower-layer children (AUFS opaque-dir)
     + attrs           source-provided attributes only (§5's round-trip rule)
```

- Interior nodes carrying blobs is a deliberate superset of git's object
  model (a git tree is either directory or leaf-blob, never both).
- **Tombstone** is first-class presence, not absence: a layer records "this
  name is deleted", so a stack can mask a name present in a lower layer.
  This is sarun's overlay whiteout promoted into the data model.
- **Opaque** is a third axis on branch nodes, not a kind of tombstone: it
  masks lower *children* — recorded and backdrop alike — while the node
  itself stays live. sarun already stores it as separate state
  (`capture.rs` `opaque` column); the layer-algebra corner cases (§6)
  force it into the model explicitly.
- **The backdrop, and holes.** A layer is a partially occluded view of a
  BACKDROP — the live host filesystem, or the empty filesystem for
  no-host stacks. The backdrop is never content in a layer; it is the
  substrate access resolves against, at access time (never snapshotted).
  Parent links between layers are an ENCODING detail: a child is stored
  as a difference from its parent, but the meaning of a layer is always
  its single composed occlusion over the backdrop. Nodes therefore carry
  an **anchor**: `Lower` (facets are a delta over the recorded occlusion
  below — the normal encoding) or `Backdrop` (this name is RE-BASED on
  the backdrop: nothing recorded below survives — not facets, children,
  or tombstones; the node's facets and explicitly-listed children are
  the entire recorded occlusion there). A pure backdrop-anchored node is
  a **hole**: "this key is not occluded." A tombstone documents
  deletion; a hole documents *lack of change* — the artifact layer
  re-encoding (rotation) leaves where the new parent-encoding contains
  changes that were never part of this layer's occlusion. Holes are
  absolute — always "backdrop", never "skip N layers"; an ancestor's
  change at a rotated name is recorded data and gets REPLICATED instead.
  Wholesale re-basing (not facet-local anchoring) is load-bearing: it is
  what keeps squash confluent — a facet-local hole meeting a squashed
  opaque would need records the squash already dropped.
- **Compose-then-apply.** Backdrop-anchored nodes make fold-application
  of a stack invalid (`apply` cannot distinguish "recorded lower" from
  "backdrop"): a stack that may contain them is resolved by composing
  all deltas first, then applying the net occlusion to the backdrop once
  (`resolve_over`). sarun's per-name top-down chain walk is exactly this
  discipline, per name.
- `attrs` carry domain metadata that is *data* (mode/mtime for fs layers, a
  wiki revision's sha1-as-recorded-by-the-source). Derived values do not
  appear here (§5).

Two semantics pinned by the reference implementation (each was forced by a
randomized-law counterexample, not chosen aesthetically):

- **Canonical form: an empty node does not exist.** A resolved node with
  no blob, no attrs, and no children is pruned; existence *is* content.
  The alternative ("a named node exists because a layer named it") makes
  compose unsound — a node materialized by one layer and emptied by a
  later one would need an inexpressible "materialize but inherit" marker
  to squash correctly. Variants that need empty directories to exist carry
  attrs on them (an fs layer's mode already does).
- **Inherit-absence.** A delta node that only inherits (blob `Keep`, attrs
  inherit, no materializing children) applied over an absent name yields
  absence, not an empty node. Corollary: the identity delta is prunable,
  and `diff` output is minimal.

## 3. The depot / bookkeeping boundary

The depot stores layers. Everything *about* layers and their owners —
box inventory, layer parentage/order, mirror fetch state, cooldowns,
watermarks, reverse indexes for dedup-on-insert (e.g. title→chain_id) —
is bookkeeping and lives in the caller's SQLite. This keeps SQLite for
exactly the data where it is good (small, relational, transactional,
inspectable with stock tools — the DESIGN.md D6 product property) and keeps
those needs from mangling the depot API.

The fence between the two, as a single criterion: **does lossless
export/import need it to round-trip?** If yes, it is layer data and appears
in the canonical encoding. If no, it is bookkeeping. Per-layer facts sit on
this fence and split by the same rule: an OCI layer's source digest is
bookkeeping (a row keyed by layer handle); a node's source-provided mtime
is data.

Precedent in-tree: `wikimak/wikipedia` already composes this way — depot
holds bytes, SQLite holds `revisions_seen` and the title↔id maps, strpool
interns strings to dense ids. That composition is the part of the crate
that was *not* sabotaged.

## 4. Canonical layer encoding — the keystone

The trait defines a **canonical, lossless, deterministic serialization of
one layer**: a deterministic walk of the node tree emitting names, blobs,
presence, opaque marks, and attrs. It is defined once and consumed three
ways:

1. **Transfer** between any two depot variants = walk source canonically →
   feed destination. No pairwise adapters.
2. **The stream variant** (§7) stores nothing *but* this encoding —
   import/export ceases to be special code and becomes "transfer to/from
   the stream variant". Correct import/export falls out of the abstraction.
3. **VBF records**: a sequence of layers serialized newest-first is exactly
   what a tiered-VBF chain stores — newest layer as the f0 record, older
   layers as refPrefix deltas in f1/cold.

Sharing is a depot-internal property, so the wire form has none; the
destination re-establishes its own sharing on ingest.

### The implicit-id rule

**Derived identifiers never appear in serialized layer data.** References
within the encoding are structural: position in the stream, back-reference
ordinals (fast-export-style marks), "unchanged relative to the previous
layer here" — never content hashes.

Why (measured, not argued): embedded object ids are high-entropy derived
bytes — incompressible in themselves, and churn amplifiers: one changed
blob rewrites the hash in every enclosing tree and the commit, turning a
10-byte edit into a cascade of fresh ~20-byte random strings no zstd window
can match against the previous layer. This is a sizable fraction of a git
repo's bulk: a zstd-compressed `git fast-export` stream is meaningfully
*smaller* than an aggressively repacked packfile of the same history
(measured on real repos, 2026-06 session). fast-export wins precisely
because it replaces embedded ids with positional marks. The same principle
is already native here: strpool ids are positional and never stored; the
VBF index is pure arithmetic.

Generalization: the serialized form contains exactly what cannot be
derived. Source-recorded checksums (a wiki revision's sha1 from the dump)
are data and round-trip; anything recomputable is omitted.

**Bit-exactness is load-bearing, not hygiene.** The view-anchored chain
form (delta records refPrefix-anchored on the previous *view's* canonical
full bytes, recomputed by the decoder from the view it just
reconstructed) only works because the encoding is deterministic and the
decoder rejects non-canonical input — encoder and decoder derive the
anchor through the same single function (`diff(None, view)` → encode).
Any nondeterminism, or an unversioned format change, makes every chain
written under the old bytes **unrecoverable**. Consequence: the golden
byte-level test is a compatibility contract; changing the canonical
encoding is a migration event for all view-anchored stores, never a
refactor.

**Stated cost** (a decision, not a surprise): dropping embedded hashes
trades away O(1) per-object integrity verification and cheap random access
into deep history. A git repo stored this way cannot verify one object in
isolation, and reading one old commit means decoding the chain down to it.
Correct trade for this workload — cold history is walked, not probed, and
the VBF access model already assumes it — and re-materializing a real git
repo (recomputing ids) is an export variant's job, paying the cost only at
the boundary where git compatibility is actually needed.

## 5. Interface shape

Two halves, because the stream variant cannot random-read or accept
out-of-order writes:

- **DepotRead** — `resolve(layer)` (the effective tree), `get(layer, name…)`,
  `walk(layer)` (canonical order), `serialize(layer)`, `diff(a, b) → Layer`,
  `squash([layers]) → Layer`.
- **DepotSink** — `put_layer(canonical stream) → LayerHandle`,
  `delete_layer(handle)`.

Random-access variants implement both; the stream variant is sink-only on
write and walk-only on read. `LayerHandle` is an opaque identity the
caller's inventory rows reference; it carries no ordering or parentage —
stacking is composed *above* the trait (the brief's layering question,
answered: the trait does not expose stacking; which layers form a box's
chain is an inventory fact in SQLite).

Method signatures are pinned only after the fs-layer variant's operation
frequencies are measured against the live overlay (per `notes/05` step 4);
the *set* above is fixed by the four workloads, the weights are not.

## 6. Layer algebra: diff, squash, rotation

`diff` and `squash` are trait-level operations. **Rotation is derived,
purely syntactic, and needs no view, no backdrop, and no host I/O**:
given boxes A (parent) and A.B (child), promoting the child —

```
B' = compose(A, B)          # new parent: carries the old stack's occlusion
A' = inverse over B's recorded footprint:
       where the old chain (A + its encoding ancestors) recorded
       something → replicate its net occlusion, re-based (backdrop-
       anchored, erasing B''s contribution);
       where it recorded nothing → a hole (backdrop shows through, LIVE)
```

Rotation rewrites encodings; no layer's occlusion changes — the same
reason dissolving a parent today does not change the view through its
children. Ancestor changes at rotated names REPLICATE (recorded data is
copyable exactly); only the backdrop must stay live, and holes preserve
exactly that. Correctness is mechanically checkable over ARBITRARY
backdrops, and these equivalences ARE the acceptance tests, generic over
every variant (checked over multiple distinct backdrops — that is the
liveness property no snapshot could pass):

```
resolve_over(bd, anc ++ [B'])     == resolve_over(bd, anc ++ [A, B])   for all bd
resolve_over(bd, anc ++ [B', A']) == resolve_over(bd, anc ++ [A])      for all bd
```

Because sharing is internal (§1), `compose` and the inverse produce
layers whose unchanged subtrees and replicated blobs alias existing
internal objects — rotation is O(changed metadata), never O(bytes). No
copy rule needs to be imposed; the definition already implies it.

The value of rotation: it restores the **newest-first invariant** —
"current state cheap, history in the tail" — across all workloads. Overlay
resolution walks child→parent, so rotating the effective/current layer to
the front makes hot lookups single-layer, the same reason VBF keeps the
newest revision alone in f0, and the fix for backwards container-layer
stacks (rotate after pull until the squashed-current layer fronts the
stack). Git history imports as layers newest-first for the same reason.

### Corner fixtures to enumerate before pinning the node model

The "tricky whiteout juggling" lives entirely in these; write them as
fixtures first:

- **Opaque inversion**: an inverse layer un-opaques by RE-BASING the
  directory on the backdrop (the backdrop children reappear live) and
  re-listing the old chain's own children explicitly.
- **Hole × tombstone**: a hole cancels recorded deletion (the tombstone
  was occlusion; the hole says "not occluded").
- **Hole under opaque**: the parent wildcard dominates — a hole beneath
  a still-opaque parent reveals nothing.
- Tombstone of a name that is itself a tombstone below.
- Metadata-only (attrs) changes, including on interior nodes with blobs.
- A tombstone in the squash base (masks nothing → vanishes vs. must be
  kept because the squash result will itself be stacked — squash of a
  *partial* stack keeps them; squash-to-root drops them).
- Interior node loses its blob but keeps children (and vice versa).

## 7. Variants

One trait, several layouts, each tuned to a shape:

| variant | layout | role |
|---|---|---|
| **hot** | "loose-git-with-whiteouts": node-per-file on the host fs, subtree/blob-level internal sharing via a private dedup index | mutable current layers; box overlays; the new lightweight store |
| **sqlar adapter** | today's `capture.rs` SQLite index + rowid-keyed blob pool | the existing store behind the trait; zero-migration first implementation, and the proof the trait fits |
| **VBF / cold** | `wikimak/depot` chains of canonically-serialized layers, newest-first, per-chain dict + refPrefix + sealing | sealed history: web archives, wikipedia, git history |
| **stream** | the canonical encoding itself, zstd-framed | import/export/transfer wire form |

Notes per variant:

- **hot** — WITHDRAWN as a variant (2026-07-05): "actually hot" is not a
  kind of storage, it is a **persistent materialization CACHE in front of
  ALL depots**. Mount-serving consumers (mmap/exec, the D5 kernel
  read-passthrough's backing fd, pread) need real loose files; those
  files are DERIVED — reconstructible from whichever depot holds the
  layer — so they form a cache with zero durability obligations: crash =
  cold cache, eviction = space management, never data loss. One
  content-keyed pool fronts every variant (dedup global by construction);
  entries are immutable and never opened writable; write = copy-up into
  the box's AUTHORITATIVE store (the existing D3 path), never cache
  mutation. Hash-naming inside the cache is maximally internal: entries
  AND the lookup index are rebuildable by re-walking depots. This is how
  a VBF-stored wiki snapshot or a lazy git ref serves a workspace mount:
  materialize on demand into the cache, pay the decode once. Distinct
  and deliberately left open: whether AUTHORITATIVE stores ever share
  bytes internally (that, not the cache, is what would make rotation
  O(metadata)) — a variant-internal choice, per §1.

- **sqlar adapter**: the seam half-exists (`BoxState`'s methods are a
  de-facto interface) but blob I/O leaks (`overlay.rs` reads `blob_path()`
  directly) and `sud.rs`/`review.rs` run raw SQL on the shared connection.
  Carving the adapter = promoting the `BoxState` surface and pulling blob
  I/O behind it. This also resolves DESIGN.md D6: rusqlite-vs-redb stops
  being a commitment and becomes a per-workload backend choice.
- **VBF/cold**: `wikimak/depot`'s chain surface (chain_id → f0/f1/cold)
  sits *below* the trait; the variant composes strpool (name → chain_id)
  + depot (chain storage) and presents layers. The un-sabotage of
  `wikimak/wikipedia` (real zstd, per-chain dict, refPrefix, sealing) is a
  prerequisite for this variant carrying real data.
- **git**: not a separate backend. Git repositories are *imported* —
  fast-export-shaped walk → canonical layers newest-first → VBF for the
  tail — and *exported* by re-materializing a repo (ids recomputed at the
  boundary). Encoding tombstones into native git trees is explicitly
  abandoned.

## 8. Read-only composition (RO attachments)

A running box may reference additional layers READ-ONLY, conceptually
stacked between its parent chain and its own upper:

```
backdrop < parent chain < RO attachments (ordered) < box upper
```

These are not a special kind of layer — they are exactly the same
objects (full layer semantics, tombstones and holes included), merely
*referenced differently* by the running box: any mutation of a key the
attachment matches is an ERROR (EROFS), rejected at open-for-write time
(a deliberate exception to first-write laziness — fail fast like a
kernel ro-mount).

The rejection is what makes attachment free: copy-up is the only path
by which lower content enters a box's layer, and it is exactly the
rejected operation — so `footprint(upper) ∩ keys(RO) = ∅` holds
structurally and the captured layer is provably independent of which
attachments were present. Attach/detach is pure bookkeeping (an ordered
list, in sqlite/meta); no re-encoding, no holes, no copying. This is
the zero-cost complement to rotation: rotation restacks layers that
interact; RO attachment is the guaranteed-non-interacting case.

Rules pinned:
- **New keys under RO-provided directories are allowed** (overlayfs-like
  dir merge): build outputs land beside RO sources, captured in the
  upper. The error set is mutation of MATCHED keys: write/truncate,
  setattr/xattr, unlink/rename-over (both need a whiteout against the
  attachment), rename-of. A layer wanting its subtree sealed against
  additions says so itself (opaque / a sealed attribute), not via the
  global rule.
- An attachment needs only the READOUT half of the trait (entry,
  children, blob read) — so anything with random-access readout can
  attach: an at-rest box, an imported canonical layer, an OCI layer, a
  git ref (served lazily from a gitdepot chain — the tip is frame 0 —
  or straight from a git repo via ls-tree/cat-file, no checkout), a
  VBF-extracted snapshot (behind materialization or a decode cache).
  The stream variant cannot attach directly (no random access).
- Relation to existing flags: `readonly_parent` is the apply-time
  cousin (child may not promote INTO parent); RO attachment is the
  write-time dual (child may not diverge FROM the attachment). Both are
  per-attachment attitudes, both bookkeeping.
- The invariant test: the same box run with and without an attachment
  produces a byte-identical captured layer (plus EROFS on matched-key
  writes, success beside them).

Use: composing SDKs, source trees, datasets, wiki snapshots into a
workspace at runtime without unpacking or copying.

## 9. Dedup needs its own observability (anti-sabotage clause)

Because sharing and compression are internal and invisible, a green
functional suite proves nothing about them — exactly how the wikipedia
encoder was gutted while its tests stayed green (`meta/reports/
vbf-recovery.md` §4). The acceptance suite MUST assert **storage
properties**, not just round-trips:

- ingest N near-identical layers → on-disk size grows sublinearly (bounded
  by the delta, not the layer size);
- rotate/squash/diff → byte growth ≈ metadata, not payload;
- VBF variant: on-disk size of a real multi-revision import ≪ uncompressed
  input (the 10–20× the design exists for).

A depot whose on-disk size matches its uncompressed input has not rendered
this design, whatever its tests say.

## 10. Build order

*Status 2026-07-05: steps 1–6 done; step 7's rotation half done (holes
in the sqlar variant, overlay walk, `rotate` verb, liveness-tested);
remaining: the materialization cache (§7 "hot", reframed 2026-07-05 —
a derived, rebuildable loose-file cache in front of all depots; the §1
internal-sharing clause stays open for authoritative stores), gitdepot moving onto depot-vbf (needs a
caller-anchored frame mode to keep the measured view-anchored hybrid),
step 8, and the git-ref RO-attachment resolver (§8).*

1. **Node model + canonical encoding + layer algebra**, as a standalone
   crate with an in-memory reference variant: encode/decode round-trip,
   diff/squash, the rotation equivalences, the §6 corner fixtures. This is
   the data model made concrete; everything else consumes it.
2. **git straightedge** (`gitdepot`): a real git repo to/from a chain of
   full-content layers newest-first, refs+commit metadata as meta,
   refPrefix-chained frames, SHA-exact export via fast-import — a second
   workload to develop the model against, and the encoding-comparison
   bench (standalone vs refPrefix chain vs solid bound).
3. **Stream variant** (nearly free once 1 exists) → transfer works; the
   sabotage-resistant size assertions get their harness.
4. **sqlar adapter** over `capture.rs` + blob pool; `overlay.rs`/`sud.rs`/
   `review.rs` stop touching `Connection`/`blob_path()` directly. Proves
   the trait against the only workload with a live consumer.
5. **Un-sabotage `wikimak/wikipedia`** (independent; one file) — verified
   by measured on-disk size on a real multi-revision page, not the
   byte-payload unit tests.
6. **VBF variant**: canonical layers into `wikimak/depot` chains; wikipedia
   and web-archive workloads land here.
7. **hot variant** (loose layout with internal sharing), then **rotation in
   anger** (container layer reordering; overlay newest-first maintenance).
8. **full git import/export** through the stream form (the straightedge tool
   generalized: signed commits, annotated tags, incremental update).

## 11. Open questions (size before committing; do not guess)

- Operation frequency weights for the fs-layer workload — measure against
  the live overlay before pinning signatures (§5).
- Name ordering: byte-lexicographic always, or variant-defined collation?
  (Canonical encoding needs ONE deterministic answer per variant.)
- Corpus magnitudes for VBF sizing — unchanged from
  `docs/tiered-vbf-and-strpool.md` §9.
- Chunking large blobs in the canonical encoding (content-defined vs fixed)
  — affects stream-variant delta quality; decide when the stream variant
  meets real container layers.
- Where the trait crate ultimately lives (`engine/` workspace vs `gimir/`)
  — start in `gimir/`, move when the sqlar adapter (step 3) needs it.
