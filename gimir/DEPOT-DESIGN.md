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
  masks lower-layer *children* while the node itself stays live. sarun
  already stores it as separate state (`capture.rs` `opaque` column); the
  layer-algebra corner cases (§6) force it into the model explicitly.
- `attrs` carry domain metadata that is *data* (mode/mtime for fs layers, a
  wiki revision's sha1-as-recorded-by-the-source). Derived values do not
  appear here (§5).

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

`diff` and `squash` are trait-level operations. **Rotation is derived, not
primitive**: given boxes A (parent) and A.B (child), promoting the child —

```
B' = squash(A, B)                 # new parent: old A.B's effective content
A' = diff(resolve(B'), resolve(A))  # new child: whiteouts for B's additions,
                                    # resurrected content for B's overwrites/deletes
```

Correctness is mechanically checkable, and these equivalences ARE the
acceptance tests, generic over every variant:

```
resolve(B' + A') == resolve(A)        # child's effective view preserved
resolve(B')      == resolve(A + B)    # parent's effective view preserved
```

Because sharing is internal (§1), `diff`/`squash` produce layers whose
unchanged subtrees and resurrected blobs alias existing internal objects —
rotation is O(changed metadata), never O(bytes). No copy rule needs to be
imposed; the definition already implies it.

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

- **Opaque inversion**: an inverse layer may need to *un*-opaque a
  directory, which has no whiteout form — the inverse must re-list the
  masked lower children explicitly.
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

- **hot**: an unpacked git repo is already a deduped node store; the delta
  is interior-node blobs + tombstones + opaque. Internal hashing for
  equal-subtree merging is fine — but hashes live only in the private
  lookup index, are derivable, hence droppable: a depot must be able to
  rebuild its dedup index from scratch with nothing observable changing.
  Subtree-level (not just blob-level) sharing is what makes `diff` prune
  whole unchanged subtrees by internal identity instead of walking them.
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

## 8. Dedup needs its own observability (anti-sabotage clause)

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

## 9. Build order

1. **Node model + canonical encoding + layer algebra**, as a standalone
   crate with an in-memory reference variant: encode/decode round-trip,
   diff/squash, the rotation equivalences, the §6 corner fixtures. This is
   the data model made concrete; everything else consumes it.
2. **Stream variant** (nearly free once 1 exists) → transfer works; the
   sabotage-resistant size assertions get their harness.
3. **sqlar adapter** over `capture.rs` + blob pool; `overlay.rs`/`sud.rs`/
   `review.rs` stop touching `Connection`/`blob_path()` directly. Proves
   the trait against the only workload with a live consumer.
4. **Un-sabotage `wikimak/wikipedia`** (independent; one file) — verified
   by measured on-disk size on a real multi-revision page, not the
   byte-payload unit tests.
5. **VBF variant**: canonical layers into `wikimak/depot` chains; wikipedia
   and web-archive workloads land here.
6. **hot variant** (loose layout with internal sharing), then **rotation in
   anger** (container layer reordering; overlay newest-first maintenance).
7. **git import/export** through the stream form.

## 10. Open questions (size before committing; do not guess)

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
