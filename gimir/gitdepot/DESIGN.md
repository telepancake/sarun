# gimir / gitdepot — Design

A local, incrementally-updated mirror of a git repository, stored as VBF delta
chains. It is:

- **SHA-exact** — every commit and tree reconstructs to a bit-identical
  serialization, so it hashes back to its exact upstream git object id.
- **Packfile-competitive** on size, by storing every lane's tree so that a
  file's variants across revisions and branches sit adjacent for zstd.
- **Cheap to serve** — any historical ref's tree is reconstructable, and the
  store updates incrementally.
- A **static musl binary**: the mirroring logic is engine code, not an external
  driver poking at the store's private format.

## Method

The design follows one rule: find the data encoding that supports the required
operations at the wanted speed/size/complexity, then write the most direct
transform over it, with the minimum of auxiliary structures. Every section below
is the encoding first, then the direct algorithm over it. There is no auxiliary
in-memory "skeleton" of the tree; the algorithms run over the byte encoding
directly.

---

## 1. Domain model: revisions, lanes, and the union

A repository is a set of branch lines that live, advance, fork, and merge over
time. At any moment only some are concurrently alive.

A **lane** is one such live line — a column in the `git log` metro-map graph.
The number of lanes at a point in time is roughly the number of *concurrently
live* branch lines, not the number of branches that ever existed; when two lanes
merge, one disappears.

The state the mirror stores at each revision is the **union of the git trees of
all live lanes** at that revision, held in a single node tree with every lane's
files side by side. That adjacency is the source of the size win: a file's
version in lane A sits next to its version in lane B, so zstd sees short
matches. (Lane assignment and lifecycle are §7; here a lane is just "one live
tree in the union.")

---

## 2. The encoding: the variant/union tree

The union is a `depot::codec` node tree. At a path, the file's distinct versions
across lanes are its **variants**, and they are stored as **sibling nodes**, not
nested under a wrapper:

- **File variant** — node named `name` + `0x00` + `varint(slot)`. `0x00` never
  occurs in a git filename, so it both delimits the slot and marks the node as a
  file variant. `slot` is a stable per-path key an editing pass assigns (§6);
  readers never interpret its value.
- **Directory** — a bare node named `name`, no tag. A node's children are thus
  either bare names (a subdirectory) or `\0`-led names (a file's variants), never
  ambiguous, and a path that is a file in one lane and a directory in another is
  representable.
- **Mode** — nothing stored for a normal file or a directory. The three
  exceptions are a single empty **mode-tag child** under the variant: `x`
  executable, `l` symlink, `m` gitlink.
- **Lane membership** — a child node named `lanes` under the variant, whose blob
  is the lane bitmap. It is a *child*, not an attr, so a lane joining or leaving
  a variant rewrites only the small bitmap, never the file content. The `lanes`
  child is **omitted when the bitmap is all-ones**: absence means "present in
  every live lane" — the common case of a file identical across branches — and a
  variant with no bitmap is the only variant at its path.

Two encoding invariants make this well-defined:

- **A single variant is still stored as a variant** (`name\0<slot>`), so the
  encoder has no one-variant special case.
- **Meta children are non-identity**: `lanes` and the mode tags carry a `Set`
  blob (empty is fine for a mode tag), so composition never prunes them as
  identity nodes and silently drops a mode or a bitmap. Their presence is the
  signal.
- **An empty node is a real object.** An empty directory is representable and
  distinct from a missing one; existence of an object is not the same as its
  absence.

---

## 3. Two orders

The union is large (hundreds of MB); the git trees it is built from are tiny.
So the union defines the order and the git trees adapt to it. There are exactly
two orders:

1. **`container_cmp` — the storage/iteration order.** The codec's bytewise order
   over the clean node names above. The union is walked in a single pass in this
   order and is never re-sorted. It is a total order over the clean names, so the
   big side can stream.
2. **git `base_name_compare` — reconstruction only.** Turning a small extracted
   level back into an actual git *tree object* to hash requires git's order,
   where a directory sorts as `name/`. This is the only place git order is used,
   and only on the tiny slice being hashed.

The two differ only in file-vs-directory prefix handling. Parsed git trees are
cached by oid and yielded in `container_cmp` order, so the sort into that order
happens once, at cache insertion, and is free on reuse. Node names stay clean —
the order lives in the comparator, never as a sort byte baked into a stored name.

---

## 4. Operations on layers: overlay, compose, delta

Layers combine through streaming functions defined **at the level of the byte
encoding** — each reads byte streams and writes a byte stream, single pass, no
tree of allocated nodes. Two markers carry deletion intent:

- a **tombstone** documents a deletion of a key;
- a **hole** documents "no change in this overlay — look through to the layer
  below" (a backdrop reference), an artifact of composing layers.

Three operations:

- **overlay (delta ∘ full-state).** Applies a delta onto a full state and
  produces a new full state. A **hole dissolves** — over a full base it becomes a
  plain removal or no-op.
- **compose (delta ∘ delta).** Stacks two deltas into one. A **hole survives**.
  This is what keeps composition associative, per the rule: `something + hole`
  composes to *nothing*, but `nothing + hole` composes to *hole*.
- **delta.** The reverse of overlay: reads two streams and writes the delta layer
  that turns the first into the second when overlaid.

The mmap updater is a thin adapter over these: given the current full-state
mmap and a delta mmap, it allocates a new mmap and makes a **single pass**,
copying from full-state or delta while re-framing the output bytes, calling
`MADV_DONTNEED` on the already-consumed front regions so memory never doubles,
then unmaps the inputs. No skeleton is built at any point.

---

## 5. Delta direction: newest full, older reverse

The newest state is stored in full; everything older is stored as a delta *from
the newer* record:

- the newest full state is **f0**;
- each older record stores the older revision of an encoded pair as a **reverse
  delta from the newer** one.

So the newest tree is read directly (one small decode), and an older tree is
reconstructed by applying reverse deltas from newer toward older. Reconstruction
never walks deltas forward to rebuild the newest tree. New content entering the
mirror is fetched by id from the git-talker; the reverse side takes the older
content by id.

---

## 6. Delta generation (write side)

Producing the delta from the previous written layer to the new one is a
**lockstep union walk** with the per-path variant algebra factored out as a
standalone unit. The two parts are separate code.

**Tree-wide (lockstep iteration).** Keep iterators over the lane trees of the
current state and of the incoming commit(s) in parallel and advance them
together, so that at each path you see which lanes have it and with what git
object id. The current state is read **straight from `refPrefix` plus the live
delta stack** (§8) by iterating them in lockstep and stacking the entries that
touch each path — no union is materialized to generate a delta.

**Per-path (reslot by oid).** For one path, working only with variant ids and
bitmaps — no tree iteration, no content bytes — reconcile the previous variant
set with the new one:

- a variant's **id** is its git `(mode, oid)`, obtained for free from the lane
  trees you are already iterating (mapped through the bitmaps). Stored content is
  never read back and hashed to recover an id.
- **Match by id.** A new variant whose id already occupies a slot keeps that
  slot; emit only a bitmap update if its membership moved — content unchanged, no
  rewrite. An id that occupies its slot but whose content changed is a
  content replacement at an **unchanged key** — a small reverse delta with good
  zstd ordering.
- an unmatched new variant takes a free slot (preferring the freed slot it
  shares the most lanes with — most likely an in-place edit — else the lowest
  free index); a vanished variant frees its slot.
- the output is the set of slot changes; the caller fetches content bytes for a
  changed slot **by id**.

---

## 7. Lanes: assignment and lifecycle

Lane ids are assigned by ancestry and are stable:

- a commit's lane is frozen from its **first parent's** already-frozen lane — a
  commit continues its ancestor's lane. If an earlier-processed sibling already
  continued that lane, the commit opens a fresh lane. Ids are minted
  monotonically and frozen at birth.
- monotonic ids are **compacted with reuse**: a lane's compact index is freed
  when the lane dies and reused by a later lane, so a bitmap over live lanes is
  only as wide as the peak concurrent lane count.
- a ref points at a commit, which carries a frozen (revision index, lane id).
  Every live ref occupies at least one lane, so **#live lanes ≥ #live refs**.

When a ref advances, the new commit's lane is chosen to **minimize the diff** to
the previous all-lanes frame, while the other still-live lanes are carried
forward until they go out of commission (merged, ref dropped, or gone stale). A
ref pointing at a commit much older than the newest (on the order of a month) is
retired into the reflog so stale branches and tags do not clog the current
state. All refs are fetched, except synthetic hidden refs a provider uses to
avoid disrupting normal git.

**Full lanes first, delta-among-lanes later.** Similar parallel lanes (measured
by the proportion of shared blob hashes) can eventually be stored as a delta
against a chosen base lane, so unbounded feature-branch fan-out does not blow up
the uncompressed context — but the base a group deltas against cannot be assumed
reconstructable forever (merge DAGs are arbitrary), so this requires switching a
branch-group's base and letting zstd absorb the reframing. This is a later
optimization; the base design stores lanes in full, side by side.

---

## 8. The geometric delta stack and the frame lifecycle

Between frame writes, deltas accumulate on a stack instead of being folded into
the full state:

- **Push and compact.** A new delta layer is pushed on the stack. Then, while the
  stack has more than one entry and the top is ≥ 70% of the size of the entry
  below it, the two topmost layers are popped, **composed** (delta ∘ delta, holes
  survive), and the result pushed back. This keeps the stack shallow with
  geometrically increasing sizes.
- **Reading the current state** is a lockstep walk of `refPrefix` plus the stack
  layers (§6, §10) — a full union is never built to read or to make the next
  delta.
- **Seal (frame write).** Writing a frame flattens the whole stack: the stack is
  collapsed and **overlaid** onto `refPrefix` (holes dissolve to removals),
  producing a new `refPrefix`, and the stack is cleared. The current lane trees
  are kept beside the frame so the next delta reads variant oids for free (§6).

---

## 9. Persistence: the VBF chains

A mirror's store is one depot instance under `<store>/depot/`, holding four
tiered VBF chains — **TREES, COMMITS, REFLOG, TAGS** — each its own vbf.

**Frame discipline (per chain).** Layers are stored **newest-first**:

- **f0** is the newest layer's canonical record, compressed standalone, so
  "read current" is one small decode.
- **f1** is the older records concatenated newest-first and compressed
  **anchored on f0's record** (the full serialized view at the newer frame's
  end), so a near-identical predecessor costs about its delta. This anchoring
  relies on bit-exact serialization.
- **Seal.** Past a size threshold, the old f1 is moved **verbatim** into a
  **cold** frame — bytes copied, never recompressed. Cold frames are write-once.

A vbf is a **solid archive read from the beginning**: there are no mid-frame
resume points, so reading history walks newest→oldest from f0.

**Stable indices.** Records are numbered from the **oldest** end; a prepend only
grows the count `N`, and an existing index never changes (frame index =
`N-1-k`). This lets a ref cite a commit and a tree by an index that survives
prepends — there are no git ids stored in refs or trees, and no `deleted_at`
markers. The kv record counts are the authoritative index base.

**Bookkeeping vs corpus.** `meta.sqlite` holds only the **current** live refs,
each pointing at a commit and a tree by stable index; the corpus is the vbf
chains. Commits live in the commits vbf in date order; superseded refs go to the
reflog vbf, not to sqlite tombstones. A **local reflog is mandatory**: history is
never deleted, so an upstream purge or rewrite cannot delete the mirror. A
non-fast-forward upstream is recorded in the reflog — the superseded commits stay
reachable — and never destructively replaces the store.

**Batch, not split.** The initial import produces essentially two frames — f0
and one f1 (immediately retired to cold if over threshold). Adding N trees at
once encodes the oldest N−1 plus the previous f0 and f1 content as deltas against
the newest tree, once, without repeating f0/f1 per tree. The seal/retire decision
is made after a prepend, never during, and never forces splitting a batch; frames
are kept as large as practical so huge f1 frames are not repeatedly recompressed.
Batching many objects into one entry also gives the compressor more to work with,
which combined with variant adjacency (§2) is where the size win comes from.

**Derived, not stored.** A sha → index map is built by one walk of the commits
chain and cached for the life of the open handle; it is discardable and cheaply
re-derived. Tree identity uses tree **indices** (already stored in refs, learned
at import), not saved tree ids. There is no schema/migration scaffolding — this
is unreleased software, so migration code would be dead and untested.

---

## 10. Serving the current state

Serving or inspecting a value is the same **k-way lockstep** over `refPrefix`
plus the live delta stack that the delta generator uses (§6). No full union is
built to read a value; a union is materialized only when a whole union is
genuinely demanded — a full read, or a seal.

---

## 11. Ingest / fetch

- **Talk git plumbing.** Piggyback on git's plumbing to exchange the higher-level
  protocol messages rather than reimplementing auth and the wire format.
- **Fetch all reachable commits and trees** (not just the first-parent chain),
  commit+tree first where that lets you evaluate and plan a large update before
  pulling blobs. Tell the remote you do not have the latest commits for each
  updated ref, so it sends the **base trees** needed for diffing — no local
  checkout, unpack, and re-diff.
- **One batch.** The initial pull is a single batch (minimizing turns avoids vbf
  splitting overhead); a daily update is then a few dozen new objects. The same
  object-ordering used for updates seeds the initial vbfs with least buffering.
- **No bare clone kept on disk** as the real store, and no checkout/diff
  reconstruction path that would lose exact edits and provenance.
- Annotated tags and gpg-signed commits are in scope for SHA-exactness (the TAGS
  chain); aborting on unsupported input is a temporary state, not the design.

---

## 12. Sharding

A mirror has a **`shard-bits`** parameter. A path is routed to one of
`2^shard-bits` shards by the **top `shard-bits` of a stable hash of its full git
path** (excluding the `\0<slot>` variant tag), so every version of a path lands
in the same shard and the split is stable across a re-shard.

- Import is **multithreaded, one thread per shard**, sharing the git object
  source but with completely separate delta/vbf state (own refPrefix, stack,
  lanes).
- **Shards advance in lockstep**: every shard writes a layer per revision, and an
  empty delta is fine and expected. Shards are not synchronized on independent
  per-shard change rates.
- Routing is **by path hash only** — never by commit (commits straddle
  directories) and never by a content-similarity heuristic.
- Sharding is a storage/locality concern and never affects identity:
  reconstructing a lane's git tree gathers that lane's entries from every shard
  and hashes them together, so the split is invisible in the tree oid.
- A later **offline re-shard** adjusts `shard-bits`. The hash function, base
  pick, and cutoffs are swappable without a format change.
