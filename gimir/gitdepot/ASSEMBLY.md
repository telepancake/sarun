# gitdepot assembly — sharded, geometric-stack, content-addressed union

The ASSEMBLY of pieces already built, into their final form — NOT a redesign.
Everything here is a thin driver over parts that exist:

| Spec item | Existing piece it uses |
|---|---|
| geometric-stack merge (§4) | `depot::stream::compose_stream` + the hole-annihilation rule |
| apply merged stack to full-state (§4) | `depot::stream::overlay_full` (the mmap driver) |
| layer iterator (§3) | single forward pass over the `depot::codec` byte grammar |
| variant tree (§2) | `gitdepot::variants` with `\0NNNN` name / mode-tag / lanes-child tweaks |
| delta / full-state bytes | `depot::codec`; VBF frames = `depot-vbf` |
| lanes / reflog (§5) | `gitdepot::lanes` + VBF metadata |

The ONLY thing dropped is the skeleton + per-seal `full_view` dead-end — the
one part never asked for.

## 0. Why the flat form (numbers)

Full-state = the flat refPrefix (written anyway) + a bounded stack of delta
layers merged geometrically; overlaid at seals with `overlay_full`. No `View`,
no skeleton. git.git rev 20000: 74-byte delta, 66.9 MiB raw refPrefix
(3.46 MiB zstd), 373 KiB skeleton — three orders of magnitude of transient
`Arc<View>` churn to emit 74 bytes was the waste.

## 1. Sharding

- Each mirror has a **`shard-bits`** parameter (fixed at import; a later
  offline re-shard process adjusts it).
- A path is assigned to a shard by the first `shard-bits` bits of a hash of
  its **full git path** (NOT including the `\0NNNN` variant tag).
- Import is **multithreaded**: one thread per shard, all sharing the same git
  object source, each with completely separate delta / VBF state.
- Bounds per-shard full-state size (no 20 TB single full-state).

OPEN: hash function (fnv/xxhash/sha-prefix?). Bits taken from which end. A
path's shard must be stable across re-shard for the offline tool to work.

## 2. Layer encoding (the union tree shape)

A file at path segment `name` is stored as one node **per variant**, siblings
named `name` + `0x00` + `varint(slot)`. A directory is named `dirname` + `0x2F`
(a trailing `/`) — see §3 for why. Per variant node:

- **content** = the node's blob (the file bytes), a `const byte range` in the
  encoding.
- **lanes** = a CHILD node (name `lanes`) whose content is the lane bitmap —
  a child, NOT an attr, so a lane join/leave rewrites only that child. OMITTED
  when the bitmap is all-ones: absence means "in every live lane" (the common
  case — a file identical across branches), so most variants store no bitmap.
  A bitmap-less variant is the ONLY variant at its path.
- **mode**: NOTHING stored for a normal file or directory. Executable file →
  an empty child node named `x`; symlink → `l`; gitlink → `m`.

No `mode` attr anywhere. Meta children (`lanes`/`x`/`l`/`m`) live UNDER a
variant node — one level below where sibling files/dirs sort — so they never
interleave with real entries (verified: agent, git `base_name_compare`).

## 3. Layer iterator

A single-pass iterator over the binary layer encoding, same interface as the
git-tree iterator, yielding in **exactly git tree object order**. Per entry:

- file path, file mode, variant index (slot), lane bitmap, **const byte range
  of the content**.

ORDER — the big side never reorders. Node names stay clean (files
`name\0<slot>`, dirs bare `name`). The container's codec bytewise order
(`layer::container_cmp`) is authoritative; the hundreds-of-MB full-state is
walked in a single pass in exactly that order, never re-sorted. The git trees
are tiny (one level), so THEY adapt: parsed trees are sorted into container
order once, at object-cache insertion (free on reuse). Only reconstructing a
git tree object for SHA sorts a small level back into git's `base_name_compare`
order (`layer::entry_cmp`). git's rule (`read-cache.c`): bytewise, but the byte
past the shorter name is `0x00` for a file, `0x2F` for a dir — so it and our
bare-dir order diverge only on file-vs-dir prefixes, a divergence confined to
the tiny git side.

### Delta generation — match by oid, never hash content

Reslot matches variants by git OID, obtained for free (git trees already carry
every entry's oid) — the container's stored content is NEVER read back and
hashed. Co-iterate by path:

- the **current state** — NOT a materialized layer: the full-state plus every
  live delta on the stack, co-iterated, and at each path the touching values
  stacked (full-state at bottom, deltas oldest→newest) by the per-path overlay
  rule → the effective current variants' slots + bitmaps. The geostack keeps
  this to ~log(n)+1 byte streams, so it stays a bounded k-way lockstep.
- the **previous lanes' git trees** and the **new lanes' git trees**.

A current variant's identity = its bitmap → any lane in it → that lane's tree
oid at this path (all lanes in a variant share the oid — that is what makes
them one variant). A new variant's identity = the new lane's tree oid here.
Match old↔new by oid, reslot, emit. Content (a `\0v` blob) is fetched from git
ONLY to write a genuinely new variant. Unchanged lanes share old==new tree oid
and are pruned in O(1); only the advancing lane's changed subtrees are walked.

Only extra state: the **lane→tree-oid map** (O(#lanes), tiny, reflog-derived) —
NOT a per-path skeleton, NOT any hashing of the full-state.

## 4. Geometric delta stack

When a delta layer has been generated and handed to the VBF code (on disk, or
kept in memory below a threshold — implementation choice), push it on a stack.
Then repeat until the stack has one entry OR the top entry is smaller than 70%
of the next entry's size:

    pop the two topmost layers, MERGE them (lower then upper), push the result.

This keeps ~log(n) layers of geometrically increasing size.

### Frame-write lifecycle

The stack accumulates and compacts deltas BETWEEN frame writes. On a frame
write (seal), the WHOLE stack is **flattened and emptied to produce the new
refPrefix**:

1. flatten the stack — `compose_stream`-merge every layer into one combined
   delta (holes survive) = `geostack.collapse(compose_stream)`.
2. `overlay_full(old_refPrefix, combined)` → the new refPrefix (holes dissolve
   to removals; positive full-state).
3. write the new refPrefix frame; **clear the stack**.

So between writes a read is `refPrefix` + the few live stack layers; at a
write the stack collapses into the refPrefix and starts fresh.

### TWO merges

There are two distinct merges, and the `something+hole→nothing` rule was these
conflated:

1. **delta ∘ delta** (compacting the geometric stack): an upper **hole
   survives** — `Content+Hole→Hole`, `Absent+Hole→Hole`, `Hole+Hole→Hole`.
   This is associative (brute-checked n=3,4) — required, since the stack's
   merge grouping is size-dependent — and is exactly `depot::stream::
   compose_stream` unchanged. (Disproof that a surviving hole is required:
   layers `[set X, set X, hole]` merged top-first vs bottom-first with
   `something+hole→nothing` give `setX` vs `nothing` — non-associative.)
2. **delta ∘ full-state** (applying the collapsed deltas at a seal): a hole
   **dissolves** to a removal, and the result is a positive full-state (no
   markers) — exactly `depot::stream::overlay_full`.

So merge = `compose_stream`, apply-to-full = `overlay_full`; both already
exist and are tested.

The two hole rules are the user's stated invariant "something+hole → nothing,
nothing+hole → hole" which preserves associativity of merge. NOTE TO SELF:
verify associativity + apply-to-full-state round-trip empirically; if a
counterexample exists (e.g. content then two holes), bring the concrete case
back rather than guessing.

## 5. Reflog

- **One reflog entry per written layer.** It explicitly records ALL lanes of
  that layer, plus any ref changes at that point.
- Each lane records a **commit index**.
- Lanes carry trees with no extra ascribed meaning, so a single delta layer
  MAY pack multiple commits — even several commits of the same ref forming one
  chain.
- WHY pack many commits per layer: variant locality. With variants stored as
  siblings (§2), packing e.g. 64 consecutive commits as 64 lanes in ONE layer
  places all 64 versions of each file adjacently, so zstd finds short local
  matches across them. Split into 64 separate layers, each file's versions land
  in different frames and that context is lost. Variant-as-sibling (§2) and
  many-commits-per-layer are the same decision: locality for the compressor.
- Invariant: **#lanes ≥ #live refs** (each live ref points at a commit → at
  least one lane).

## 6. Component / dependency order

1. **Encoding + iterator** (§2, §3) — foundation; everything reads/writes it.
2. **Merge** (§4 algebra) over the encoding + iterator; then the geometric
   stack driver.
3. **Delta generation**: lockstep iterate full-state + stack + git trees →
   new delta.
4. **Sharding harness** (§1): per-shard threads over 1–3.
5. **Reflog** (§5).
6. Wire into `LaneStore`-equivalent + SHA-exact proof; then swap blob
   acquisition (`cat-file` → `fetch-pack --shallow-since` + `unpack-objects`).
