# gimir / gitdepot — Recovered Design (from the user's own words)

> Recovered by reading the user's turns in the session transcript, grounded
> against the code already on disk (`gitdepot/src/{store,frame,layer,geostack,
> reslot,lanes,reflog,shards}.rs`, `depot-vbf`, `depot-stream`). Where the
> assistant and the user disagreed, **the user is authoritative** and the
> assistant's version is flagged as a correction so later agents don't
> re-introduce it. This is a spec, not a transcript.

---

## 0. The user's design method (verbatim, load-bearing)

> "I first try to find a data encoding that would support the operations I
> need and have the desired speed/size/complexity tradeoff, then design an
> algorithm that transforms input data into output data in the most direct,
> least complex and resource intensive way, using minimum extra data
> structures needed. Again, based on desired speed/size/complexity."

Everything below follows from this method: pick the encoding first, then write
the *most direct* transform over it with the *minimum* auxiliary structures.
The recurring failure mode the user fought was the assistant reaching for a
heavyweight general mechanism (in-memory "skeleton"/"skel" nodes, full-union
materialization on every commit, per-record union rebuilds, O(n²) merges,
schema/migration scaffolding, extra "keyframes", separate driver binaries)
instead of the minimal data-first transform. Do not do that.

## 1. The goal

A **local, incrementally-updated git-repo mirror** stored in gimir (VBF delta
chains):

- **SHA-exact** — every commit and tree reconstructs to its exact upstream git
  object id (bit-identical serialization; otherwise history is unrecoverable).
- **Competitive with packfiles** on size (measured, not asserted), by letting
  zstd exploit cross-revision and cross-branch redundancy.
- Serves **any historical ref's tree cheaply**, incrementally updatable.
- Ships as a **static musl binary** — the mirroring logic is *engine code*, not
  a bolted-on external driver poking at the store's private format.

---

## 2. The data encoding — the variant/union tree

The unit stored per written layer is a **union of the git trees of all live
lanes at that revision**, encoded as a `depot::codec` node tree. One node tree
holds every lane side-by-side so zstd sees all variants of a file adjacent.

### 2.1 Node shapes (user's exact instructions, C166–C168, C195, C198)

- **Files are stored as sibling variants**, not nested under a "keep" node.
  A file at segment `name`, occupying slot `k`, is the node named
  `name` + `0x00` + `varint(k)`. `0x00` never occurs in a git filename, so it
  both delimits the slot and marks the node as a *file variant*.
- **Directories are bare** — node name = the directory name, no slot tag.
  (Directory names never get a variant tag appended.)
- **Mode is NOT stored inline** and is NOT an attr. For a normal file or a
  directory, no tag at all. For the exceptions, a single empty **mode-tag child
  node** under the variant: `x` = executable, `l` = symlink, `m` = gitlink.
- **Lanes bitmap is a child node** of the variant (named `lanes`), not an attr —
  specifically so that a lane joining/leaving a variant is a *bitmap-only*
  update, not a rewrite of file content. "Do not store mode and bitmap inside
  file content ... so that adding a new lane is not 100% update of all file
  content, just the bitmaps."
- **Omit the `lanes` child when the bitmap is all-ones** (C198): absence means
  "present in every live lane" — the common case (a file identical across all
  branches), so most variants store no bitmap. A bitmap-less variant is
  necessarily the only variant at its path.
- **Meta nodes must be non-identity.** `lanes` and the mode tags carry a `Set`
  blob (empty is fine for a mode tag) so `compose`/`overlay` don't prune them as
  identity `[0,0]` children and silently drop a mode or bitmap. Their *presence*
  is the signal.
- **Always store variants, even when there's only one** (C167) — "to make your
  first version free of special cases." A single-variant path is still a
  `name\0<slot>` node.

The user explicitly chose this over the earlier "variants live inside a keep
node" idea: variants are **neighbours in the same directory**, which makes the
tree walk a plain union walk (see §5).

### 2.2 Empty nodes / the "sentinel" correction (C133–C144)

The assistant's canonical-form rule "an empty node (no children, no blob) ==
nonexistent → None" is a **data-loss bug** for this use: it cannot represent an
empty directory, and git *does* have empty-dir semantics to respect. The user
rejected the "collapse empty → nonexistent" rule outright ("who came up with
this nonsense... existence of a concrete object is not equal to no object").
Empty directories must be representable. Do not re-introduce the collapse rule.

---

## 3. Two orders (never conflate them)

There are exactly two orderings, and the "big side never reorders":

1. **`container_cmp` — the authoritative iteration order.** The container's
   codec bytewise order over the clean node names is THE order the full-state
   (the union, hundreds of MB) is walked in, in a single pass, **never
   re-sorted**. The layer iterator emits the container in this order; the small
   git trees adapt to it.
2. **git `base_name_compare` — only for reconstruction.** Turning a small
   extracted level back into an actual git *tree object* to hash sorts that
   level into git's `base_name_compare` order (a directory sorts as `name/`).
   This is the *only* place git order is used, and only on tiny slices.
3. **git trees are sorted once, at cache insertion.** Parsed git trees are
   cached by oid; they are yielded in `container_cmp` order, and because they're
   cached the sort happens once and is free on reuse.

`container_cmp` differs from git order only in the file-vs-dir prefix handling;
it is a total order over the clean names so the big side can stream. The user's
correction here (C196): when the git iterator produced wrong order, the fix is
**"fix the git iterator to always produce correct order"**, NOT to bake a sort
byte (like a trailing `/`) into stored names. Names stay clean.

---

## 4. Two merges — compose vs overlay; holes vs tombstones

Two merge operations over layers, both single-pass over the byte encoding:

- **delta ∘ delta = `compose`** (stacking two deltas). **Holes survive.** The
  associativity requirement the user stated (C195): `something + hole` merges to
  *nothing*, but `nothing + hole` merges to *hole*. Preserving holes here is
  what keeps the merge associative.
- **delta ∘ full = `overlay`** (applying a delta onto a full-state). **Holes
  dissolve** — an overlay onto a full base turns a hole into a plain removal /
  no-op, producing a new full-state.

**Hole vs tombstone** (from the earlier depot algebra, C17–C19, still the
governing distinction): a *tombstone* documents a deletion; a *hole* documents
*lack of change in the summary overlay* — "this key is not occluded, look into
the backdrop." Holes are artifacts of layer composition, not multi-hop skips.

The user demanded these two operations exist **at the level of the stable byte
encoding** as fundamental functions in the depot codebase (C190): one function
reads two byte streams and writes the overlay of the second on the first; a
second reverse function writes the *delta* layer that turns the first stream
into the second when applied. The mmap driver is a *tiny adapter* over these
streaming functions — "AT NO POINT YOU NEED ANY SKELETON." (These are
`depot::stream::compose_stream` / `overlay_full` / the delta generator.)

---

## 5. The delta-generation algorithm (write side)

Per revision (per shard), the delta from the previous written layer to the new
one is produced by a **lockstep union walk**, with the per-path variant algebra
factored out as a standalone unit. This is the algorithm the user dictated
step-by-step (C168, C174, C176–C179, C200–C201) and repeatedly had to defend
against "combined tree"/"union materialize"/"skeleton" reinterpretations.

### 5.1 Tree-wide part (lockstep iteration)

- Keep iterators of **all lane trees of the current state and of the next
  commit(s)** in two parallel vectors. Advance them in lockstep so that for
  every path you see which lanes have that path and with what git object id.
- The **current state is read straight from `refPrefix` + the live delta stack**
  by iterating them in lockstep and correctly stacking the entries that touch
  each path. **No union is materialized** to generate a delta (C190, C192,
  C201). Materializing a multi-GB frame with tens of millions of objects on
  every commit is explicitly forbidden.
- This tree walk is completely separate code from the per-path logic.

### 5.2 Per-path part — reslot by oid (C176, C200)

For a single path, in isolation (no tree iteration, no depot types, no content
bytes — just variant ids and bitmaps):

- You have the previous variants (each an id = git blob oid, or oid+mode, plus a
  lane bitmap) occupying slots, and the new variant set (id → bitmap).
- **Match variants by id.** A new variant whose id already occupies a slot stays
  in that slot; emit a bitmap update only if its bitmap moved (content
  unchanged, no rewrite). This is an in-place edit → slot reused → the frame
  carries a content replacement at an unchanged key (small reverse delta, good
  zstd ordering).
- Unmatched new variants are assigned to slots (heuristically, to the freed slot
  with most lane overlap — "most likely an edit of aaaa"), lowest free slot
  index preferred. Vanished variants free their slots.
- The output is the set of slot changes; the caller fetches content bytes for a
  changed slot **by id**.

**Reslot-by-oid, never hash stored content** (C200, critical correction): the
git object ids are *already on the trees you are iterating*. You get each
variant's oid for free from the **lane trees** (previous encoded layer's lanes
and the new layer's lanes) via the lane-tree iterators mapped through the
bitmaps. Do **not** read the full-state's stored bytes and hash them to recover
oids at every commit. The assistant's "read reslot state from the full-state's
own bytes" was rejected.

### 5.3 Delta direction — f0 full, everything else reverse (C178–C179, C59)

The **newest full state is f0**; all other records store the **older** revision
of each encoded pair as a delta *from the newer* one. Newest tree up front,
older trees forming the tail. The cold tier reconstructs *older* trees by
applying reverse deltas — it does **not** reconstruct the newest tree by walking
deltas (if it did, "you encode all your deltas backwards and your whole
implementation is broken", C59). New content is fetched by id from the
git-talker; the reverse side takes old content by id.

---

## 6. Lanes — persistent, ancestry-based

"Lane" = a metro-map lane in `git log`: at any point in time the number of lanes
in the vbf equals (roughly) the number of concurrently live branch lines, not
the number of branches that ever existed (C122, C127). Merged lanes disappear.

- **Persistent, ancestry-frozen assignment** (C132, code `lanes.rs`): a commit's
  lane is frozen from its **first parent's** already-frozen lane — keep a commit
  in its ancestor's lane; an advanced commit inherits the ancestor's lane. If an
  earlier-processed sibling already took that lane, open a fresh one. Ids minted
  monotonically, frozen at birth, never reused as *ids*.
- **Compaction with reuse**: monotonic ids are remapped to a compact space that
  *reuses* an index once a lane dies, so a bitmap over live lanes is only as wide
  as the peak concurrent lane count.
- A ref points to a commit; the commit carries a frozen (revision index, lane
  id). Every live ref occupies at least one lane, so **#live lanes ≥ #live
  refs** (enforced in `reflog.rs`).
- The problem to solve when a ref advances (C132): pick the lane id for the new
  commit that **creates the smallest diff** from the previous all-lanes frame —
  while still carrying the other *live* lanes until they go out of commission
  (merged, ref dropped, or stale).
- Stale-ref retirement (C122): retire to reflog any ref pointing at a commit
  older than ~a month before the newest commit, so stale branches/tags don't
  clog the current state. Fetch **all** refs (minus synthetic hidden refs the
  provider hides to avoid disrupting normal git, C129).

### 6.1 Variant similarity / delta-of-delta (C122–C123) — deferred, not dropped

The user's model: decide how similar two parallel lanes are by the proportion of
equal blob hashes in their trees (many same files → variant; few → independent),
and store variant branches as a delta against an arbitrarily-chosen base lane so
you don't blow up the uncompressed context (you can't fit all of linux's feature
branches side by side without delta-among-them). This needs **sound
delta-of-delta** under git's requirements.

**Correction/scope (C123, C161):** the "base lane must stay live and
reconstructable forever" invariant is *not generally achievable* — git merge
DAGs are arbitrary; you must implement **switching the branch-group base** and
let zstd eat the reframing cost. And explicitly: get **full (non-delta-encoded)
lanes correct first**, delta-of-delta later. The current encoding stores lanes
in full side-by-side; delta-among-lanes is a later optimization.

---

## 7. The geometric delta stack + frame lifecycle (C195, C197, C187)

The write path never materializes the union to make a delta. Instead:

- **Geometric stack of deltas.** After a new delta layer is produced and handed
  to the vbf code (on disk, or in memory under a threshold — implementer's
  choice), push it on a stack. Then repeat: while the stack has >1 entry **and**
  the top entry is ≥70% of the next entry's size, pop the two topmost layers,
  **merge them in the correct order** (`compose` = delta∘delta, holes survive),
  and push the result back. This keeps the stack shallow with geometric sizes.
- **Reading current state** = iterate `refPrefix` + all stack layers in
  lockstep (§5.1). A full union (`overlay_full(refPrefix, collapse(stack))`) is
  materialized **only** when a full union is genuinely needed — a read or a seal
  — never to generate the next delta (`frame.rs::union`).
- **Frame write / seal** (C197): writing a frame **completely flattens and
  empties the stack** to produce a new `refPrefix`. A **seal** replaces
  `refPrefix` with the collapsed union (holes dissolve to removals) and clears
  the stack. Lane trees of the current state are kept alongside the frame so the
  next delta can read a variant's oid for free (§5.2).
- **The mmap updater** (C187): given two anon-mmap areas (current full state, and
  the delta blob), alloc a new anon-mmap, do a **single pass** over both copying
  from either previous full-state or delta while correctly re-framing the new
  full-state bytes; periodically `MADV_DONTNEED` the already-processed front
  regions to avoid doubling memory; at the end unmap old full-state + delta and
  present the new full-state. This is a thin adapter over the streaming
  overlay/delta functions — **no skeleton, no skel nodes** (see §9 corrections).

---

## 8. Storage / persistence model — the VBF chains (this DOES exist)

This is central and was wrongly dismissed by the assistant as "doesn't exist."
It is real: `depot-vbf` + `store.rs`.

- A mirror's store is `<store>/depot/` — **one** wikimak-depot instance holding
  **four tiered VBF chains**: `TREES=0`, `COMMITS=1`, `REFLOG=2`, `TAGS=3`
  (`store.rs`). Trees, commits, reflog, and tags each get their own vbf (C54,
  C68, C78).
- **VBF frame discipline** (`depot-vbf/src/lib.rs`): layers stored
  **newest-first**.
  - **f0** = the newest layer's canonical record, standalone zstd → "read
    current" is one small decode.
  - **f1** = older records concatenated newest-first, zstd **refPrefix-anchored
    on f0's record** — a near-identical successor costs ~the delta. The user's
    advertised modification (C48): refPrefix is the **full serialized depot view
    at the end of the newer frame**, anchored on the reconstructed view bytes,
    which only works given **bit-exact** serialization (C11).
  - **Seal** (`seal_f1`): past the seal threshold the old f1 moves **verbatim**
    into a **cold** frame — the SPEC's seal invariant. Old frame bytes are
    copied verbatim; nothing is recompressed. Cold frames are **write-once**.
- **VBF is a solid archive read only from the beginning** — there are no "resume
  points" mid-frame (C97). Reading history walks newest→oldest from f0.
- **Stable oldest-first indices** (C54, C82, `store.rs`): records are numbered
  from the **oldest** end. Prepends only *grow* the count `N`; an existing
  index never changes. Frame index = `N-1-k`. This is what lets refs cite a
  commit/tree by a stable index that survives prepends — **no git ids in refs or
  trees, no `deleted_at`** (C54). The kv counts (`n_trees`, `n_commits`, …) are
  the authoritative index base.
- **Bookkeeping = sqlite, corpus = vbf** (C52, C68): `meta.sqlite` holds only
  **current** live refs, each pointing to a commit **and** a tree by their
  stable oldest-first index. Commits live in the commits vbf in **date order**;
  reflog lives in the reflog vbf. Old refs are written to the reflog vbf, not
  kept as tombstones in sqlite.
- **Never delete local history / local reflog is mandatory** (C52): you must keep
  a local reflog, else an upstream purge/rewrite would delete your mirror. A
  non-fast-forward upstream must **not** replace the store; it is recorded in the
  reflog, keeping the superseded commits reachable in the chain.
- **Batch, don't split** (C55, C60, C89, C96, C203): the initial full import
  produces essentially **two frames** — f0, and one f1 that is immediately
  retired to cold if it's over the threshold. When adding N new trees at once you
  do **not** repeat f0/f1 per tree; you encode the oldest N-1 plus the previous
  f0 and previous f1 content as deltas against the newest tree, **once**. The
  seal/retire decision is made **after** a prepend, never during, and never
  forces splitting a batch. The f1 uncompressed-size threshold is for *retiring
  f1 to cold*, not for forbidding large frames — "we want frames as big as
  possible, we do not want to recompress those huge f1 frames repeatedly" (C95).
- **Packing for compression** (C68, C202–C203): batch multiple objects into one
  vbf entry so refPrefix "has some meat to work with." Because variants of a
  file are stored adjacent (§2), 64 consecutive commits in 64 lanes give zstd
  far better context than 64 separate layers — shorter matches. One frame holds
  as many layers as is practical.

### 8.1 Derived indices (C69–C74)

- **sha → idx** is an in-RAM map built by **one** object-level walk of the
  commits chain, cached for the life of the open store handle; a ref-name attach
  doesn't build it, a sha attach walks once per process. The **commit** map is
  on-demand and discardable — cheaply re-derivable by walking the commit vbf
  once; keep nothing persistent this cheap.
- You do **not** need to save tree ids: use **tree indices** instead — they're
  already stored in refs alongside the commit idx, and you learn a commit's tree
  idx when you import it (C73). Tree hashes are bit-exact-derivable from normal
  (uncooked) repos.
- **No schema/migration scaffolding** (C70): this is unreleased software; adding
  "schema N→N+1 eager migration" now just creates dead, never-run, never-tested
  code. Forbidden.

---

## 9. Sharding (C116, C119, C195 §shards)

- Each git-repo mirror has a **`shard-bits`** parameter. Paths are routed to one
  of `2^shard-bits` shards by the **top `shard-bits` of a stable hash of the full
  git path** (NOT including the `\0<slot>` variant tag), so every version of a
  path lands in the same shard and the split is stable across re-shard.
- Import is **multithreaded, one thread per shard**; all threads share the same
  git object source but each keeps **completely separate delta/vbf state** (its
  own refPrefix, delta stack, lanes).
- **Shards advance in lockstep** (C119, key correction): every shard writes a
  layer per revision; **empty deltas are OK and GOOD**. The assistant's "shards
  advance on real change at independent rates" was rejected outright — "you will
  go bald ... good luck debugging your independent-rates sync bugs." Do NOT sync
  shards on independent rates.
- **Never shard by commit; never shard by path-similarity heuristics** (C116,
  C114): shard by path hash only. Commits straddle directories, and all shards
  carry equal (possibly empty) deltas anyway to avoid housekeeping.
- Sharding is a storage/delta-locality concern only — it never changes identity.
  Reconstructing a lane's git tree gathers that lane's entries from **every**
  shard and hashes them together (§3); the split is invisible in the tree oid.
- A later **offline re-shard** process adjusts `shard-bits`. Tunables like
  base-pick, cutoff, and the hash function must be swappable **without a format
  change or rewrite** (C131) — "this stuff does not affect format."

---

## 10. Current-state read (no materialization)

Serving/inspecting the current state is a **k-way lockstep** over the
full-state (`refPrefix`) plus the live delta stack — the same lockstep the delta
generator uses (§5.1). No full union is built to read a value; the union is
materialized only when an actual full union is demanded (a read of the whole
thing, or a seal). This is the "read current state via lockstep, no
materialization" pillar the user insisted on against repeated
"materialize-the-union" reinterpretations (C190–C195).

---

## 11. Ingest / fetch (C84–C121, C161)

- **Talk git plumbing, don't reimplement the wire proto** (C84, C92): piggyback
  on git plumbing to shuffle higher-level proto messages; don't reimplement
  auth/wire from scratch.
- Fetch **all reachable commits and trees** (not just the first-parent chain,
  C85), commit+tree only where useful so you can **evaluate and plan** big
  updates (C115) before pulling blobs. During fetch, tell the remote you don't
  have the latest commits for each updated ref so it **sends the base trees** you
  need for diffing — no recomputation, no local checkout/unpack-and-diff (C161).
- **Initial pull is one batch**, not iterated incremental updates (C89): always
  do the minimum number of turns (one), or unnecessary vbf splitting overhead
  becomes prohibitive. A daily update is then just a few dozen new objects
  copied in (C86). The same object-ordering trick used for updates seeds the
  initial vbfs with least buffering (C88).
- **No "pragmatic third path"** that reconstructs via checkout/diff and loses
  exact edits and provenance (C44). No bare mirror clone left on disk as the real
  store (C46, C78, C87) — if you keep a full `repo.git` bare clone, "why are we
  implementing git mirroring?"
- Annotated tags and gpg-signed commits are in scope for SHA-exactness (the
  `TAGS` chain exists); a clean abort on unsupported input is a temporary state,
  not the design (C79, C81).

---

## 12. Corrections the assistant kept violating (do not repeat)

1. **"append" for what is actually "prepend"** (C42–C43). Newest-first storage
   means new revisions are **prepended**, never appended. The user had this
   "eradicated twice" and it kept coming back.
2. **Materializing the union / "skeleton" / "skel" nodes on every commit**
   (C190, C192–C195). The delta is generated by lockstep iteration of
   `refPrefix` + stack; the union is materialized only for a read or a seal.
   No auxiliary skeleton tree of malloc'd nodes.
3. **Reslot by hashing stored full-state content** (C200). Match variants by git
   `(mode, oid)` read for free from the lane trees; never hash stored content to
   recover oids.
4. **Empty node == nonexistent (the "sentinel"/collapse rule)** (C133–C144).
   Data-loss bug; empty directories must be representable. Rejected.
5. **Shards advancing at independent rates** (C119). Shards advance in lockstep
   with empty deltas allowed. Shard by path hash only, never by commit or by
   content-similarity.
6. **meta.json / full-file rewrite of all refs+commits; `deleted_at`
   tombstones; schema+migration scaffolding; one-store-per-ref** (C51, C53, C54,
   C70). Bookkeeping is compact sqlite of current refs by stable index; corpus
   is the vbf chains; no migrations in unreleased software.
7. **Discarding the user's design to build a different engine** (C59, C111,
   C159, C165, C175, C216). The recurring pattern: build an over-engineered
   thing, declare the user's idea unworkable, keep the pet engine. The user's
   design (this document) is authoritative.

---

## 13. Provenance note

Sections above are attributable to specific user turns (cited inline as C-numbers
from the recovered user-turn stream). The code on disk already implements much of
this (`frame.rs`, `layer.rs`, `reslot.rs`, `lanes.rs`, `reflog.rs`, `shards.rs`,
`store.rs`, `depot-vbf`) and corroborates the encoding, the two orders, the two
merges, reslot-by-oid, lockstep read, lane persistence, the reflog invariant, and
the four-chain / f0-f1-seal-cold VBF persistence. Later agents: adapt terminology
to this document, delete anything that re-introduces a §12 violation.
