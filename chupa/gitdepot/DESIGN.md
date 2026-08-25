# gimir / gitdepot — Design

A local, incrementally-updated mirror of a git repository, stored as VBF delta
chains.

- **SHA-exact** — the stored serialization is bit-identical to git's, so a
  reconstructed commit or tree hashes back to its exact upstream object id.
- **Packfile-competitive** on size, by storing every lane's tree so a file's
  versions across revisions and branches sit adjacent for zstd.
- **Cheap current state** — the latest commit and tree of each live ref is a
  small decode; the store updates incrementally. (Older history is
  reconstructable, but it is not the fast path.)
- Ships as a single **static musl binary**.

## Method

Find the data encoding that supports the required operations at the wanted
speed/size/complexity, then write the most direct transform over it with the
minimum of auxiliary structures. Each section is the encoding first, then the
direct algorithm over it. The algorithms run over the byte encoding directly;
there is no auxiliary in-memory tree of nodes.

---

## 1. Domain model: revisions, lanes, and the union

A repository is a set of branch lines that live, advance, fork, and merge over
time; only some are concurrently alive at any moment.

A **lane** is one such live line — a column in the `git log` metro-map graph.
The number of lanes at a point in time is roughly the number of *concurrently
live* branch lines, not the number of branches that ever existed; when two lanes
merge, one disappears.

The state stored at each revision is the **union of the git trees of all live
lanes** at that revision, held in one node tree with every lane's files side by
side. That adjacency is the size win: a file's version in lane A sits next to its
version in lane B, so zstd sees short matches. (Lane assignment is §7; here a
lane is just "one live tree in the union.")

---

## 2. The encoding: the variant/union tree

The union is a `depot::codec` node tree. At a path, the file's distinct versions
across lanes are its **variants**, stored as **sibling nodes**, not nested under
a wrapper:

- **File variant** — node named `name` + `0x00` + `varint(slot)`. `0x00` never
  occurs in a git filename, so it both delimits the slot and marks the node as a
  file variant. `slot` is a small per-path key (§6): the smallest free number,
  meaning nothing on its own.
- **Directory** — a bare node named `name`, no tag. A node's children are thus
  either bare names (a subdirectory) or `\0`-led names (a file's variants), never
  ambiguous; a path that is a file in one lane and a directory in another is
  representable.
- **Mode** — nothing for a normal file or a directory. The three exceptions are a
  single empty **mode-tag child** under the variant: `x` executable, `l` symlink,
  `m` gitlink.
- **Lane membership** — a child node named `lanes` under the variant, whose blob
  is the lane bitmap. It is a *child*, not an attr, so a lane joining or leaving a
  variant rewrites only the small bitmap, not the file content. The `lanes` child
  is **omitted when the bitmap is all-ones**: absence means "present in every live
  lane" — the common case of a file identical across branches — and a variant with
  no bitmap is the only variant at its path.

Encoding invariants:

- **A single variant is still stored as a variant** (`name\0<slot>`), so there is
  no one-variant special case.
- **Meta children are non-identity**: `lanes` and the mode tags carry a `Set`
  blob (empty is fine for a mode tag), so composition never prunes them as
  identity nodes and drops a mode or a bitmap. Their presence is the signal.
- **An empty node is a real object**: an empty directory is representable and
  distinct from a missing one.

---

## 3. One order

Everything internal is walked in a single order: the codec's bytewise order over
the clean node names above (`container_cmp`). The union is walked in one pass in
this order and never re-sorted.

The git trees the mirror diffs against are reordered into this order when they
are loaded into the tree-object cache, so every iteration over them is already in
the one order — the sort happens once per object, on load, and is free on reuse.
The loading context knows a tree object's full path, so a blob's shard (§9) is
assigned during that same cache load.

---

## 4. Operations on layers: overlay, compose, delta

Layers combine through streaming functions defined at the level of the byte
encoding — each reads byte streams and writes a byte stream in a single pass,
with no tree of allocated nodes. Two markers carry deletion intent:

- a **tombstone** documents a deletion of a key;
- a **hole** documents "no change in this overlay — look through to the layer
  below" (a backdrop reference), an artifact of composing layers.

Three operations:

- **overlay (delta ∘ full-state)** applies a delta onto a full state and produces
  a new full state. A **hole dissolves** — over a full base it becomes a plain
  removal or no-op.
- **compose (delta ∘ delta)** stacks two deltas into one. A **hole survives**.
  This keeps composition associative: `something + hole` composes to *nothing*,
  but `nothing + hole` composes to *hole*.
- **delta** is the reverse of overlay: it reads two streams and writes the delta
  layer that turns the first into the second when overlaid.

The mmap updater is a thin adapter over these: given the current full-state mmap
and a delta mmap, it allocates a new mmap and makes a **single pass**, copying
from full-state or delta while re-framing the output, calling `MADV_DONTNEED` on
the already-consumed front regions so memory never doubles, then unmaps the
inputs.

---

## 5. The frame model: refPrefix and the geometric delta stack

Between frame writes, deltas accumulate on a stack rather than being folded into
the full state:

- **`refPrefix`** is the current full state (a full union).
- **Push and compact.** A new delta layer is pushed on a stack. Then, while the
  stack has more than one entry and the top is ≥ 70% of the size of the entry
  below it, the two topmost layers are popped, **composed** (delta ∘ delta, holes
  survive), and the result pushed back. This keeps the stack shallow with
  geometrically increasing sizes.
- **Current state** is `refPrefix` plus the live stack, read by lockstep
  iteration (§6) — a full union is never built just to read a value or to make
  the next delta.
- **Seal (frame write).** Writing a frame flattens the whole stack: it is
  collapsed and **overlaid** onto `refPrefix` (holes dissolve to removals),
  yielding a new `refPrefix`, and the stack is cleared.

---

## 6. Delta generation (write side)

The delta from the previous written layer to the new one is a **lockstep walk**,
with the per-path variant algebra factored out as a separate unit.

**Tree-wide (lockstep of three iterator sets).** Advance in parallel, by path:

1. the git trees of the lanes of the **last encoded layer**;
2. the git trees of the lanes of the **layer being encoded**;
3. the **last encoded layer as stored** — reconstructed by lockstep over
   `refPrefix` + the delta stack (§5).

At each path this shows which lanes have it and with what git blob object id, on
both the previous side (sets 1 + 3) and the new side (set 2). No union is
materialized to generate a delta.

**Per-path (reslot).** For one path, working only with slots, bitmaps, and the
blob object ids from the lane trees — no content bytes:

- reconcile the previous variants (each a slot + lane bitmap) with the new
  variant set. **Two variants are the same iff their blob's git `(mode, object
  id)` match** — read for free from the lane trees (sets 1 and 2), never by
  hashing stored content.
- a variant whose `(mode, object id)` still exists keeps its slot; if only its
  lane membership changed, emit a bitmap update.
- a new `(mode, object id)` is placed into a **freed slot** — one whose old
  variant just vanished — choosing the freed slot it shares the **most lanes**
  with, on the heuristic that it is an in-place edit of that slot's former file,
  so the slot key stays put. If no freed slot fits, it takes the smallest free
  slot. It stores the full blob content, fetched by object id.
- a vanished variant with no successor frees its slot.

A variant always holds the full blob content; slots are arbitrary small numbers
whose only job is to be a stable key across revisions.

---

## 7. Delta direction: newest full, older reverse

The newest state is stored in full; everything older is a delta *from the newer*
record. The newest full state is **f0**; each older record stores the older
revision of an encoded pair as a **reverse delta from the newer** one. So the
newest tree is read directly, and an older tree is reconstructed by applying
reverse deltas from newer toward older through the stack/overlay machinery (§5).
Reconstruction never walks deltas forward to rebuild the newest tree. New content
is fetched by object id; the reverse side takes the older content by object id.

An update re-creates the previous f0 as a delta layer by the same mechanism as
any other older layer. The previous f0 is then discarded — an update never reads
it again except as the refPrefix used to unpack the f1 it prepends to.

---

## 8. Lanes: assignment and lifecycle

Lane ids are assigned by ancestry and are stable:

- a commit's lane is frozen from its **first parent's** already-frozen lane — a
  commit continues its ancestor's lane. If an earlier-processed sibling already
  continued that lane, the commit opens a fresh one. Ids are minted monotonically
  and frozen at birth.
- monotonic ids are **compacted with reuse**: a lane's compact index is freed
  when the lane dies and reused by a later lane, so a bitmap over live lanes is
  only as wide as the peak concurrent lane count.
- a ref points at a commit, which carries a frozen (revision index, lane id).
  Every live ref occupies at least one lane, so **#live lanes ≥ #live refs**.

A lane **dies** when its line ends: its ref is merged into another lane or
dropped, or the ref has had no new commit for a long stretch and is declared
**inactive** — retired into the reflog so a stale branch or tag does not keep
hogging a lane. When a ref advances, the new commit's lane is chosen to
**minimize the diff** to the previous all-lanes frame while the other still-live
lanes are carried forward. All refs are fetched, except synthetic hidden refs a
provider uses to avoid disrupting normal git.

---

## 9. Sharding

The tree union is partitioned across shards so no single shard holds the whole
repository's tree state. A mirror has a **`shard-bits`** parameter; a path is
routed to one of `2^shard-bits` shards by the **top `shard-bits` of a stable hash
of its full git path** (excluding the `\0<slot>` variant tag), so every version
of a path lands in the same shard and the split is stable across a re-shard.

- Import is **multithreaded, one thread per shard**, sharing the git object
  source but with completely separate tree/delta state per shard (own refPrefix,
  stack, lanes).
- **Shards advance in lockstep**: every shard writes a layer per revision, and an
  empty delta is fine and expected. Shards are not synchronized on independent
  per-shard change rates.
- Routing is **by path hash only** — never by commit (commits straddle
  directories) and never by a content-similarity heuristic.
- Sharding never affects identity: reconstructing a lane's git tree gathers that
  lane's entries from every shard, so the split is invisible in the tree oid.
- A later **offline re-shard** adjusts `shard-bits`. The hash function, base pick,
  and cutoffs are swappable without a format change.

---

## 10. Persistence: the VBF chains

Storage is one depot per mirror. The sharded **tree** state is stored per shard;
the repo-global **commit**, **reflog**, and **tag** streams each have their own
chain.

**Frame discipline (per chain).** Layers are stored **newest-first**:

- **f0** is the newest layer's record, compressed standalone, so "read current"
  is one small decode.
- **f1** is the older records concatenated newest-first and compressed **anchored
  on f0's record** (the full serialized view at the newer frame's end), so a
  near-identical predecessor costs about its delta. The anchoring relies on
  bit-exact serialization.
- **Seal.** Past a size threshold the old f1 is moved **verbatim** into a **cold**
  frame — bytes copied, never recompressed. Cold frames are write-once.

A vbf is a **solid archive read from the beginning**: there are no mid-frame
resume points, so reading history walks newest→oldest from f0.

**Stable indices.** Records are numbered from the **oldest** end; a prepend only
grows the count `N`, and an existing index never changes (frame index = `N-1-k`).
A ref can therefore cite a commit and a tree by an index that survives prepends —
no git ids are stored in refs or trees, and there are no `deleted_at` markers.

**Bookkeeping vs corpus.** `meta.sqlite` holds only the **current** live refs,
each pointing at a commit and a tree by stable index; the corpus is the vbf
chains. Superseded refs go to the reflog chain, not to sqlite tombstones. A
**local reflog is mandatory**: history is never deleted, so an upstream purge or
rewrite cannot delete the mirror — a non-fast-forward upstream is recorded in the
reflog (its superseded commits stay reachable) and never destructively replaces
the store.

**Batch, not split.** The initial import produces essentially two frames — f0 and
one f1 (retired to cold if over threshold). Adding several trees at once encodes
the older ones plus the previous f0 and f1 content as deltas against the newest
tree, once, without repeating f0/f1 per tree. The seal/retire decision is made
after a prepend, never during, and never forces splitting a batch; frames are
kept as large as practical so huge f1 frames are not repeatedly recompressed.

---

## 11. Ingest / fetch

The point of fetching is to get exactly the commits and trees the encoder needs,
using git's own negotiation rather than a reimplemented wire protocol.

- **Withhold the boundary from the advertisement.** The mirror advertises that it
  does *not* have the last encoded commits of each updated ref. The remote
  therefore sends those commits **and their git tree objects** — the boundary
  trees the diff runs against. This is why the encoder always has bit-exact tree
  bytes to diff and to hash, and why no local checkout, unpack, and re-diff (and
  no tree reserialization, §3) is ever done.
- **Metadata first.** Fetch commit+tree metadata for the reachable set before
  pulling blobs, so a large update can be evaluated and planned first.
- **One batch.** The initial pull is a single batch; minimizing turns avoids vbf
  splitting overhead. Later updates are incremental over the same negotiation.
- **No bare clone** is kept on disk as the real store, and there is no
  checkout/diff reconstruction path that would lose exact edits and provenance.
- Annotated tags and gpg-signed commits are in scope for SHA-exactness (the tag
  chain); aborting on unsupported input is a temporary state, not the design.
