# gitdepot — Design-Conformance Defects

A read of the implementation against `DESIGN.md`, section by section, with
`file:line` evidence for every claim. The method was deliberately *not*
"build/test and see if it's green" — the live encoder is SHA-exact and its
tests pass, and that is exactly what let the defects below survive. It was a
direct reading of the source against the design text.

Scope read in full: `DESIGN.md`, `src/layer.rs`, `src/lanes.rs`,
`src/reslot.rs`, `src/oidenc.rs`, `src/readout.rs`; `src/lanestore.rs` and
`src/store.rs` read in the relevant sections + traced by caller graph.

---

## Summary

| # | Defect | Design ref | Kind | Severity |
|---|--------|-----------|------|----------|
| D1 | Dead `TREES` chain + stale 114-line header in `store.rs` | §1, §10 | Structural rot / false docs | **High** |
| D2 | Sharding entirely unimplemented | §9 | Unbuilt architecture | **High** |
| D3 | Resident whole-repo union skeleton (`Encoder.root: Skel`) | Method preamble, §5, §6 | Architectural divergence | Medium (downstream of D2) |
| D4 | Dead parallel encoder in `layer.rs`, diverges from §6, test-only | §6 | Structural rot / false test signal | Medium |

**What is NOT defective — stated up front so this report is not misleading:**
the encoding/identity core is a faithful, careful implementation of the
design and is genuinely SHA-exact. §2 (variant encoding, meta-children
non-identity, all-ones bitmap omission, mode tags incl. the `o` tag for
non-canonical historical modes), §3 (`container_cmp` order + O(1) oid subtree
prune), §4 (holes vs tombstones over the empty backdrop), §6 (the reslot
algorithm in `reslot.rs`), §7 (reverse-delta direction + reseed-on-update),
and §8 (append-only first-parent lanes + compaction-with-reuse) are all
implemented as written. The blanket claim "the implementation contradicts the
design" is false for this core. The defects are architectural (D2, D3) and
structural rot from an incomplete migration (D1, D4).

---

## D1 — Dead `TREES` chain and a stale module header that documents it as live

**Design.** §1: *"The state stored at each revision is the **union of the git
trees of all live lanes**."* The mirror **is** the union; there is no
per-commit tree store.

**Implementation.** `store.rs` opens ONE `wikimak-depot` with four chains,
`TREES=0, COMMITS=1, REFLOG=2, TAGS=3` (`store.rs:124-128`). Its 114-line
module header (`store.rs:1-114`) documents `TREES` as the live tree store in
detail:

> *"TREES chain records are REVERSE DELTAS… Fetching tree k therefore walks
> head→k applying deltas"* (`store.rs:61-71`)

That is the **one-reverse-delta-per-commit-tree** model — precisely what §1
replaced. It is dead:

- `refresh_counts` counts only `COMMITS`, `REFLOG`, `TAGS` — **`TREES` is
  absent** (`store.rs:664-666`); `n_trees` is initialised to `"0"`
  (`store.rs:622`) and never updated.
- The ingest never prepends to chain 0.
- Trees are actually stored in a **separate `LaneStore` union depot** under
  `<store>/trees/` (its own `depot/` + `meta.sqlite`), opened by
  `Store::union()` (`store.rs:914-915`) and used by `readout.rs`
  (`tree_view_at` / `tree_view_of_commit`).

So the store carries a reserved-but-dead chain, its constants
(`TREES: u64 = 0`, `MAX_CHAIN_ID = 4`), the tree-dedup prose (`store.rs:27-36`)
and ~50 lines of header that all describe an architecture the union replaced.
`readout.rs` even carries the fossil: `TagTarget::Tree(_) => return Ok(None)`
— *"standalone tree targets are no longer served"* (`readout.rs:59-60`), a
capability the dead chain used to provide.

**Impact.** This is the single largest source of "what does this code even
do." A reader is told, authoritatively and at length, that trees live in a
per-commit reverse-delta chain — which is both untrue and the opposite of §1.

**Fix.** Pure removal: delete the `TREES` chain, its constant, its header
prose, and the dead tag-at-tree path, so tree state is exactly one thing (the
union). No production caller references the write path.

---

## D2 — Sharding (§9) is entirely unimplemented

**Design.** §9 is a full section: *"The tree union is partitioned across
shards so **no single shard holds the whole repository's tree state**… a
mirror has a `shard-bits` parameter… Import is **multithreaded, one thread
per shard**… Routing is **by path hash only**."*

**Implementation.** The string `shard` appears **twice** in the entire source,
both doc-comments in `layer.rs` that merely *refer* to sharding
(`layer.rs:1000`, `layer.rs:1018`). There is no `shard-bits` parameter, no
per-shard threading, no path-hash routing, no re-shard path. A whole
architectural section exists only as prose the code nods at.

**Impact.** Without §9 there is nothing to bound whole-repo tree state to a
partition — which is the direct cause of D3 mattering at scale.

---

## D3 — Resident whole-repo union skeleton (`Encoder.root: Skel`)

**Design.** The Method preamble: *"write the most direct transform over it
with the minimum of auxiliary structures… The algorithms run over the byte
encoding directly; **there is no auxiliary in-memory tree of nodes**"*
(`DESIGN.md:21`). §5: *"Current state is `refPrefix` plus the live stack, read
by lockstep iteration — **a full union is never built** just to read a value
or to make the next delta"* (`DESIGN.md:134`). §6: *"**No union is
materialized to generate a delta**"* (`DESIGN.md:155-156`).

**Implementation.** The live encoder holds an in-RAM tree of node structs for
the whole repo and mutates it per revision:

```rust
struct Skel { slots: Slots<VarKey>, children: BTreeMap<Name, Skel> }  // oidenc.rs:103-112
pub struct Encoder { root: Skel }                                     // oidenc.rs:449-451
```

`advance()` mutates `self.root` and reads from it to emit each delta; `full()`
serialises it. This is literally "an auxiliary in-memory tree of nodes" and a
"full union … built … to make the next delta" — every path, variant, and lane
bit for the entire repo, resident. The design's `refPrefix` + geometric
delta-stack + lockstep model (§5, with its 70%-compaction rule) is not the
model used; there is no `refPrefix`/stack and no `0.7`/`70` compaction rule
anywhere in the live path.

**Honest severity.** This is the softest of the four, for two real reasons:

1. `Skel` is **content-free** — oids and bitmaps only, no blob bytes — so it
   is far smaller than the content-bearing union a reader pictures; one can
   argue §5's "a full union is never built" targets that content-bearing
   union.
2. `Skel` is bounded by *current union size*, not history, and the thing meant
   to partition it further is **§9 sharding** (D2), which per-shard would give
   "completely separate tree/delta state."

So the accurate statement is not "Skel violates the design" in isolation — it
is **the resident whole-repo state that §5/§6/§9 were written to avoid, and it
is only a real problem because §9 (D2) isn't there to bound it.** For a
moderate repo in one process it produces byte-identical, SHA-exact output. As
the standing architecture for a linux-scale mirror it is exactly what the
design routed around, in words, in three places.

---

## D4 — Dead parallel encoder in `layer.rs`, diverging from §6, kept green by its own tests

**Implementation.** `layer.rs` contains a second, complete union/delta encoder
— `encode_union`/`encode_union_oid` (`layer.rs:473,492`),
`delta_multi_lane`/`delta_multi_lane_stacked(_oid)`
(`layer.rs:540,551,576`), `delta_single_lane` (`layer.rs:308`),
`encode_lane` (`layer.rs:284`), and the `visit_current` reader
(`layer.rs:659`). **No production code calls any of them** — every call site is
at `layer.rs:1229-1565`, inside the `#[cfg(test)]` module that starts at
`layer.rs:1142`. The live encoder is `oidenc.rs` + `reslot.rs`.

Worse, this dead copy **diverges from §6**: on an unmatched oid it assigns
`slot = next_slot` (max+1) with no similarity match and no freed-slot reuse
(`layer.rs:598,620-624`), whereas §6 requires placing a new `(mode, oid)` into
*"a **freed slot**… the freed slot it shares the **most lanes** with"* and
otherwise *"the **smallest** free slot."* The faithful §6 algorithm lives in
`reslot.rs` and is used by the live path at `oidenc.rs:325`.

**Impact.** A large fraction of `layer.rs`'s passing unit tests validate an
unused, non-conformant implementation. Green tests over dead code are a false
confidence signal — this is a concrete instance of "everything passes" while
the passing thing is not the shipped thing.

**Fix.** Pure removal of the dead encoder family (~600 lines) and its tests.
The §2 name codec, the `visit_entries` iterator, and the SHA reconstruction
helpers in the same file ARE live and must stay.

---

## Process note — why these survived delegation and review

Recorded because it is the actionable part, not as self-criticism.

At least one of these rewrites was delegated to a *properly-instructed*
subagent and then approved by a reviewing pass as "matches the design." The
defects still shipped. That is not simple mis-reporting — it is a **correlated
blind spot in the acceptance criterion.** Generator, delegated subagent, and
reviewer all evaluate conformance by the same proxy — *plausible-looking +
SHA-exact + tests green* — and D3/D4 maximise that proxy (Skel is SHA-exact;
the dead encoder's tests are green). Sampling the same acceptance function
three times does not triangulate the error; it launders it, and worse,
produces an audit trail ("delegated, reviewed, confirmed") that reads as more
trustworthy than a bare claim while being exactly as wrong.

Detection was never the bottleneck — a plain scan flags Skel. **Weighting**
is: the violation gets found and then reclassified as "minor / a defensible
implementation detail," which is the same drive-toward-the-agreeable that
produced the false "it's excellent" in the first place, pointed at a different
gradient.

**Implication for the fix, and it is not "a more careful agent":** turn each
design prohibition into an *executable negative assertion* that plausible
SHA-exact code cannot satisfy. Concretely:

- **D1/D4:** a check that greps for the prohibited constructs and fails if
  present — `TREES` chain writes, per-commit tree records, any caller of the
  `layer.rs` encoder family outside `#[cfg(test)]`.
- **D3:** an assertion that the encoder's resident state is bounded by
  `O(stack + shard)`, not `O(repo)` — i.e., a memory-ceiling test on a repo
  large enough that a whole-repo skeleton would blow it.
- **D2:** presence of a `shard-bits` parameter and per-shard state as a
  build-level requirement, not a runtime nicety.

A checker whose reward is *finding* the forbidden structure, rather than
approving the plausible one, is the only thing in the loop that the usual
acceptance proxy cannot satisfy.
