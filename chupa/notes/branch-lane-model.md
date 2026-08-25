# The branch-lane git store model (design of record, 2026-07)

Supersedes the per-commit whole-tree TREES chain (walk_order + reverse
deltas). That design was wrong for its main job: reading a ref's
current tree was O(history), and parallel branches forced deltas that
oscillate between unrelated trees. This is the replacement, arrived at
in design review. **Subtree/path sharding is explicitly deferred** —
everything below treats the tree as one unit.

## Fetch (settled — no SHA juggling)

- Fetch **all** refs the server advertises (not just heads+tags); the
  only exclusions are forge pseudo-refs the server itself hides
  (`refs/pull/*` etc.).
- Ask for a **normal, non-thin** fetch: haves = our stored tip commit
  SHAs, wants = advertised tips. A non-thin pack is self-contained, so
  there is NO thin-pack base problem — the entire stub-contract /
  `materialize_snapshots` / `blob:none` / want-by-oid apparatus is
  removed. We do not enumerate tree/blob SHAs.
- `git index-pack` the pack → O(1) local random access to every
  object. Encode into the store reading objects from the local index
  in whatever order the frame wants (no promisor round-trips, no
  reverse-frontier, no staging). Delete the pack after.
- Peak disk = pack + store; fits by construction (you don't mirror a
  repo bigger than your disk). At rest we keep only the depot + tip
  commit SHAs (for the next negotiation's haves).

## Current-state hygiene: stale-ref retirement

The "current state" = the head frame's live lanes. A ref (branch or
tag) whose tip commit is older than ~1 month before the repo's newest
commit is **retired**: its binding is written to the reflog and it
stops occupying a live lane. Retirement is NOT deletion — the tree
data stays in history and the ref resolves through reflog/history on
demand (never-delete invariant, same as upstream purge/force-push).
This bounds the live-lane count to actual active development.

## Lanes from topology (the "branch" prefix)

A branch prefix is a **metro lane of the commit graph**, not a ref
name. A lane is born at a branch point, lives while it develops
concurrently, dies when it merges. The number of live lanes at a
revision ≈ the width of `git log --graph` there, computed from the DAG
(`rev-list --parents`). Refs pin into lanes; the lane set is
topological.

## Variant grouping (similarity)

Two live lanes are **variants** or **independents** by the fraction of
shared blob oids between their trees:
`|blobs(A) ∩ blobs(B)| / |blobs(A) ∪ blobs(B)|` — pure oid-set
intersection on the fetched trees, no content reads. High → variant
cluster; low → independent (its own base).

## Encoding

Two axes, composed (delta-of-delta):

- **Temporal (within a lane):** reverse deltas along the revision
  axis, **lockstep** across lanes with **empty deltas** where a lane
  didn't move. Empties compress to ~nothing and are REQUIRED: lockstep
  = one shared revision index addresses every lane directly, so a
  commit's tree is "every lane at index i" — no per-lane version
  mapping, no sync bugs. Independent rates are forbidden (they
  reintroduce exactly that fragile mapping).
- **Variant (across lanes in a cluster):** one lane is the
  (arbitrary) base; the others are stored as deltas **against the base
  at the same revision**, not as their own full content. This does the
  cross-lane dedup EXPLICITLY — you cannot fit all of a big repo's
  feature branches side by side in one 128 MB zstd window and rely on
  the window to dedup them.

Reconstruct variant lane L's tree at revision i: reconstruct base B's
tree at i (temporal walk), then apply L's variant delta at i.
Base-first, then variant; temporal within each. **One canonical
composition order.** Whatever the layering, the leaf must be
**byte-exact** git objects (canonical tree encoding, exact modes/oids,
exact blob bytes); every reconstructed oid is asserted against the
fetched one (as `git_obj_oid` already does).

## Base-switching (the hard part)

"Keep the base alive as long as variants reference it" is NOT
achievable — arbitrary merge DAGs + an unknowable future mean any base
choice is provisional. So base death is normal and switching is
mandatory:

- A base dies when its metro lane ends (merges) while variants are
  still live — the death is the **switch trigger**, read off topology.
- Promote a surviving variant to base: reconstruct its full state,
  materialize it as the new anchor, re-express the other survivors as
  deltas against it **going forward**. That re-expression is a
  **reframe** — a bounded, one-time boundary recompression that zstd
  eats.
- Initial base choice is arbitrary precisely because switching exists.

Soundness across switches:
- **Past is immutable.** Pre-switch revisions keep their deltas
  against the old base; the old base's states stay reconstructable in
  history (dying removes it from the LIVE set, not the store). Old
  revisions reconstruct against the old base forever.
- **Base-in-effect is pinned per switch boundary.** Reconstruction at
  revision i selects the base current at i: old base below the
  boundary, new base above it. The promoted lane changes mode at the
  boundary (delta-against-old below → self-anchored base above); its
  own past stays delta-against-old.
- A switch only moves the base pointer; it never invalidates an
  existing frame. delta-of-delta stays sound because each revision has
  exactly one base-in-effect and one composition path, leaf-oid
  asserted.

## Soundness obligations (what the encoding must guarantee)

1. Every reconstructed tree/blob is byte-exact git → oid matches the
   fetched object; asserted at reconstruction.
2. `compose` (the depot Layer algebra) is associative and the
   canonical codec round-trips — the basis for both delta axes; the
   299-seed property tests extend to the two-axis + base-switch case.
3. Retirement/lane-death respect base lifetime via switching, never by
   dropping a referenced base into a hole.
4. Reflog retirement is recoverable and never destroys data.

## Deferred (do NOT design yet)

Subtree/path sharding (per-`arch/mips`, per-`drivers/usb` chains).
Same lockstep + empty-delta discipline will apply spatially later;
out of scope until the whole-tree lane model is sound.
